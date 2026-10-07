//! A closed stdout is a normal way for a reader to stop (AGT-1634):
//! `pm list | head -1`, quitting `less` early, `grep -q`. Rust ignores
//! `SIGPIPE`, so the next write fails with `EPIPE` and `println!` panics
//! with "failed printing to stdout: Broken pipe". `main` routes every verb
//! through [`run`], which turns that into a quiet exit with
//! [`exit::BROKEN_PIPE`] (`128 + SIGPIPE`, the status a shell reports for a
//! writer the signal would have killed) instead of a panic message.
//!
//! The default `SIGPIPE` disposition is not restored instead: `pm app`
//! writes to its ui-leaf child's stdin, and a dead child must surface as an
//! error there, not kill pm silently.

use std::any::Any;
use std::io;
use std::panic;
use std::process::ExitCode;

use crate::exit;

/// The prefix std's `print!`/`println!` panic with when the write fails.
const PRINT_PANIC: &str = "failed printing to stdout";

/// Run `body` (the whole CLI), exiting quietly with [`exit::BROKEN_PIPE`]
/// if stdout's reader went away. Any other panic keeps std's default
/// report and unwinds as before.
pub fn run(body: impl FnOnce() -> ExitCode + panic::UnwindSafe) -> ExitCode {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if !is_broken_stdout(info.payload()) {
            default_hook(info);
        }
    }));
    match panic::catch_unwind(body) {
        Ok(code) => code,
        Err(payload) if is_broken_stdout(&*payload) => ExitCode::from(exit::BROKEN_PIPE),
        Err(payload) => panic::resume_unwind(payload),
    }
}

/// Whether a verb's error is a write to a closed pipe (a writer that
/// propagates the `io::Error` rather than panicking).
pub fn is_broken_pipe(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
    })
}

/// Whether a panic payload is std's stdout-write panic for `EPIPE`. The
/// payload is only the formatted message, so this matches its text: the
/// `print!` prefix plus `EPIPE`'s `io::Error` rendering.
fn is_broken_stdout(payload: &(dyn Any + Send)) -> bool {
    let msg = match payload.downcast_ref::<String>() {
        Some(s) => s.as_str(),
        None => match payload.downcast_ref::<&str>() {
            Some(s) => s,
            None => return false,
        },
    };
    let epipe = io::Error::from(io::ErrorKind::BrokenPipe).to_string();
    msg.starts_with(PRINT_PANIC) && (msg.contains(&epipe) || msg.contains("Broken pipe"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_only_the_stdout_epipe_panic() {
        let epipe: Box<dyn Any + Send> = Box::new(format!(
            "{PRINT_PANIC}: {}",
            io::Error::from_raw_os_error(32)
        ));
        assert!(is_broken_stdout(&*epipe));
        let other: Box<dyn Any + Send> = Box::new(format!("{PRINT_PANIC}: Bad file descriptor"));
        assert!(!is_broken_stdout(&*other));
        let unrelated: Box<dyn Any + Send> = Box::new("Broken pipe");
        assert!(!is_broken_stdout(&*unrelated));
    }

    #[test]
    fn finds_broken_pipe_anywhere_in_an_error_chain() {
        let err =
            anyhow::Error::from(io::Error::from(io::ErrorKind::BrokenPipe)).context("writing");
        assert!(is_broken_pipe(&err));
        assert!(!is_broken_pipe(&anyhow::anyhow!("Broken pipe")));
    }
}
