//! The connections write transactions run on (AGT-1463, oaudit r2).
//!
//! A push or a seed-end is one transaction, and a transaction needs a
//! connection to itself. Until AGT-1463 every write in every workspace
//! went through one connection behind one mutex, so one workspace's run
//! of large pushes stalled every other workspace. Now writes are
//! serialised **per workspace** and run on a small pool:
//!
//! 1. [`Writers::acquire`] first takes the workspace's in-process lock
//!    (a `tokio::sync::Mutex`, FIFO), so at most one write per workspace
//!    is in flight in this process and queued writers hold no connection;
//! 2. only then does it take a connection from the pool
//!    ([`WRITER_CONNECTIONS`]), waiting if every one is busy with another
//!    workspace.
//!
//! Ordering is still the database's job: every write transaction locks
//! the workspace's row (`FOR NO KEY UPDATE`, see `ops` and `numbers`)
//! before it takes sequence values, which is what makes seqs
//! commit-ordered per workspace — across replicas, admin processes and
//! anything else that writes. The in-process lock only stops one busy
//! workspace from parking every pooled connection on that row lock.

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::{Mutex, OwnedMutexGuard, Semaphore, SemaphorePermit};
use tokio_postgres::Client;

/// Write connections the server opens. Each distinct workspace writing at
/// once uses one; a workspace never uses more than one.
pub const WRITER_CONNECTIONS: usize = 4;

pub struct Writers {
    idle: StdMutex<Vec<Client>>,
    /// One permit per connection in `idle`.
    permits: Semaphore,
    /// Per-workspace write locks, present while someone holds or awaits
    /// one (removed by the last [`WorkspaceLock`] to drop).
    workspaces: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
}

/// A pooled connection, held with its workspace's write lock. Derefs to
/// the [`Client`]; returns it to the pool on drop.
pub struct Writer<'a> {
    client: Option<Client>,
    writers: &'a Writers,
    _permit: SemaphorePermit<'a>,
    _workspace: WorkspaceLock<'a>,
}

struct WorkspaceLock<'a> {
    writers: &'a Writers,
    workspace: String,
    guard: Option<OwnedMutexGuard<()>>,
}

impl Writers {
    pub fn new(clients: Vec<Client>) -> Self {
        assert!(!clients.is_empty(), "at least one writer connection");
        Writers {
            permits: Semaphore::new(clients.len()),
            idle: StdMutex::new(clients),
            workspaces: StdMutex::new(HashMap::new()),
        }
    }

    /// The workspace's write lock, then a connection (see the module doc).
    pub async fn acquire(&self, workspace: &str) -> Writer<'_> {
        let lock = self
            .workspaces
            .lock()
            .expect("workspace lock map")
            .entry(workspace.to_string())
            .or_default()
            .clone();
        let workspace_lock = WorkspaceLock {
            writers: self,
            workspace: workspace.to_string(),
            guard: Some(lock.lock_owned().await),
        };
        let permit = self.permits.acquire().await.expect("never closed");
        let client = self
            .idle
            .lock()
            .expect("idle writers")
            .pop()
            .expect("a permit means an idle connection");
        Writer {
            client: Some(client),
            writers: self,
            _permit: permit,
            _workspace: workspace_lock,
        }
    }

    /// Workspaces with a write held or queued (tests).
    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.workspaces.lock().unwrap().len()
    }
}

impl Deref for Writer<'_> {
    type Target = Client;
    fn deref(&self) -> &Client {
        self.client.as_ref().expect("present until drop")
    }
}

impl DerefMut for Writer<'_> {
    fn deref_mut(&mut self) -> &mut Client {
        self.client.as_mut().expect("present until drop")
    }
}

impl Drop for Writer<'_> {
    fn drop(&mut self) {
        // Back to the pool before the permit (a field, dropped after this)
        // lets the next writer take it. A transaction dropped without
        // commit has already queued its ROLLBACK on this connection, which
        // runs before the next writer's first statement.
        if let Some(client) = self.client.take() {
            self.writers.idle.lock().expect("idle writers").push(client);
        }
    }
}

impl Drop for WorkspaceLock<'_> {
    fn drop(&mut self) {
        drop(self.guard.take());
        let mut map = self.writers.workspaces.lock().expect("workspace lock map");
        // Clones are only made under the map lock, so a count of one (the
        // map's own) means nobody holds or awaits this workspace's lock.
        if map
            .get(&self.workspace)
            .is_some_and(|l| Arc::strong_count(l) == 1)
        {
            map.remove(&self.workspace);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The locking without a database: the pool is exercised through
    /// the workspace locks alone, since a `Client` needs a server.
    fn locks() -> Writers {
        Writers {
            idle: StdMutex::new(Vec::new()),
            permits: Semaphore::new(0),
            workspaces: StdMutex::new(HashMap::new()),
        }
    }

    async fn lock<'a>(w: &'a Writers, ws: &str) -> WorkspaceLock<'a> {
        let l = w
            .workspaces
            .lock()
            .unwrap()
            .entry(ws.to_string())
            .or_default()
            .clone();
        WorkspaceLock {
            writers: w,
            workspace: ws.to_string(),
            guard: Some(l.lock_owned().await),
        }
    }

    #[tokio::test]
    async fn one_workspace_waits_for_itself_but_not_for_others() {
        let w = locks();
        let a = lock(&w, "a").await;
        // Another workspace is not blocked by a's holder.
        let b = tokio::time::timeout(Duration::from_secs(1), lock(&w, "b"))
            .await
            .expect("b does not wait for a");
        // a itself is.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), lock(&w, "a"))
                .await
                .is_err()
        );
        drop(b);
        assert_eq!(w.tracked(), 1, "b's entry is gone");
        drop(a);
        assert_eq!(w.tracked(), 0, "the map does not grow");
        let _again = tokio::time::timeout(Duration::from_secs(1), lock(&w, "a"))
            .await
            .expect("released");
    }
}
