//! The ticket editor's JavaScript (AGT-1403): `crates/pm/tests/views/*.test.ts`
//! run under node (`node --test`, no browser) against a real `pm app` in a
//! temp workspace — two editor sessions converging through the API, a CLI
//! `pm edit`/`pm set` reaching an open one — then `pm doctor` on what they
//! left behind.
//!
//! node is not a required toolchain for pm: without a `node` (>= 22, for
//! TypeScript type stripping) on `PATH` this test prints why and passes.
//! `PM_REQUIRE_NODE_TESTS=1` turns that skip into a failure. The same
//! command by hand: `cargo test -p pm --test views_js -- --nocapture`.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// `node` on PATH if it is new enough to strip types, else why not.
fn node() -> Result<PathBuf, String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let node = std::env::split_paths(&path)
        .map(|dir| dir.join("node"))
        .find(|p| p.is_file())
        .ok_or("no node on PATH")?;
    let out = Command::new(&node)
        .arg("--version")
        .output()
        .map_err(|e| format!("{}: {e}", node.display()))?;
    let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let major: u32 = version
        .trim_start_matches('v')
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .ok_or_else(|| format!("unreadable node version {version:?}"))?;
    if major < 22 {
        return Err(format!("node {version} is older than 22"));
    }
    Ok(node)
}

fn assert_ok(out: &Output) {
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `pm` with a cleared environment: HOME the sandbox, never a real
/// ui-leaf or browser.
fn pm(home: &Path, ws: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_pm"));
    cmd.args(args)
        .arg("--workspace")
        .arg(ws)
        .env_clear()
        .env("HOME", home)
        .env("USER", "tester")
        .env("TMPDIR", home)
        .env("PATH", "/usr/bin:/bin")
        .env("UI_LEAF_NO_OPEN", "1")
        .stdin(Stdio::null());
    cmd
}

struct Kill(Child);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn the_editor_binding_converges_against_a_real_server() {
    let node = match node() {
        Ok(node) => node,
        Err(why) => {
            if std::env::var("PM_REQUIRE_NODE_TESTS").is_ok_and(|v| v == "1") {
                panic!("PM_REQUIRE_NODE_TESTS=1 but {why}");
            }
            eprintln!("skipping the view JS tests: {why}");
            return;
        }
    };

    let home = tempfile::tempdir().unwrap();
    let ws = home.path().join("ws");
    let run = |args: &[&str]| pm(home.path(), &ws, args).output().unwrap();
    assert_ok(&run(&["init", "--prefix", "AGT", "--preset", "saltline"]));
    assert_ok(&run(&[
        "new",
        "--title",
        "Original title",
        "--description",
        "line one\nline two",
    ]));

    // A short idle grace: once the tests close their streams, the server
    // exits by itself — the window closing.
    let mut child = pm(home.path(), &ws, &["app", "--json", "--idle", "1"])
        .env("PM_ACTOR", "tester:app")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let mut app = Kill(child);
    let launch: Value = serde_json::from_str(&line).unwrap();

    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/views");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&tests)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().ends_with(".test.ts"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no tests in {}", tests.display());
    let out = Command::new(&node)
        .args(["--experimental-strip-types", "--no-warnings", "--test"])
        .args(&files)
        .env("PM_APP_URL", launch["url"].as_str().unwrap())
        .env("PM_APP_TOKEN", launch["token"].as_str().unwrap())
        .env("PM_BIN", env!("CARGO_BIN_EXE_pm"))
        .env("PM_WORKSPACE", &ws)
        .env("PM_TEST_HOME", home.path())
        .output()
        .unwrap();
    // node's TAP report, for `--nocapture`.
    eprintln!("{}", String::from_utf8_lossy(&out.stdout));
    assert_ok(&out);

    // Every stream is closed: the server winds down on its own, cleanly.
    let start = Instant::now();
    let status = loop {
        if let Some(status) = app.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "pm app did not exit after the views closed"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(0));

    // What the sessions wrote is ordinary history.
    assert_ok(&run(&["doctor"]));
    let show = run(&["show", "AGT-1", "--json"]);
    assert_ok(&show);
    let ticket: Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(ticket["title"], "Set from the CLI");
    let description = ticket["description"].as_str().unwrap();
    for part in [
        "A: ",
        "B was here",
        "[a]",
        "[b]",
        "from the CLI",
        "from the window",
        "retried: ",
    ] {
        assert!(
            description.contains(part),
            "{part:?} missing: {description:?}"
        );
    }
}
