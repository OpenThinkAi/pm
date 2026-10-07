//! A reader that closes stdout early (`pm list | head -1`) ends pm quietly
//! (AGT-1634): no "failed printing to stdout" panic on stderr, exit `141`
//! (`128 + SIGPIPE`, `docs/cli-contract.md` §Exit codes).

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use tempfile::TempDir;

/// Well past any pipe buffer (64 KiB on Linux and macOS), so the writer is
/// still writing when the reader goes away.
const TICKETS: usize = 1500;
const DESCRIPTION_LINES: usize = 20_000;

struct Sandbox {
    home: TempDir,
    ws: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let ws = home.path().join("ws");
        let sb = Sandbox { home, ws };
        let out = sb
            .cmd(&["init", "--prefix", "AGT", "--preset", "saltline"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        sb
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pm"));
        cmd.args(args)
            .arg("--workspace")
            .arg(&self.ws)
            .env_clear()
            .env("HOME", self.home.path())
            .env("USER", "tester")
            .stdin(Stdio::null());
        cmd
    }

    /// AGT-1 carries a description far larger than a pipe buffer, and
    /// enough tickets follow it that `pm list` overflows one too. A batch
    /// file is YAML, and JSON is YAML.
    fn seed_large(&self) {
        let description = "a line of description text\n".repeat(DESCRIPTION_LINES);
        let mut tickets = vec![serde_json::json!({"title": "big", "description": description})];
        tickets.extend((0..TICKETS).map(|i| {
            serde_json::json!({"title": format!("filler ticket {i} with a title long enough to fill the pipe")})
        }));
        let path = self.home.path().join("batch.yaml");
        std::fs::write(&path, serde_json::json!({ "tickets": tickets }).to_string()).unwrap();
        let out = self
            .cmd(&["new", "--batch", path.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Run `args`, read one line of stdout, close it, and return the exit
    /// code and stderr.
    fn head_one(&self, args: &[&str]) -> (Option<i32>, String) {
        let mut child = self
            .cmd(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut first = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut first)
            .unwrap();
        assert!(!first.is_empty(), "`pm {}` printed nothing", args.join(" "));
        // The reader is dropped here: stdout's only read end is closed.
        let out = child.wait_with_output().unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

#[test]
fn closed_stdout_exits_quietly_for_every_large_output() {
    let sb = Sandbox::new();
    sb.seed_large();
    for args in [
        &["list"][..],
        &["list", "--json"],
        &["show", "AGT-1"],
        &["show", "AGT-1", "--json"],
    ] {
        let (code, stderr) = sb.head_one(args);
        assert!(
            !stderr.contains("panicked") && !stderr.contains("failed printing"),
            "`pm {}` panicked on a closed stdout: {stderr}",
            args.join(" ")
        );
        assert_eq!(stderr, "", "`pm {}` wrote to stderr", args.join(" "));
        assert_eq!(code, Some(141), "`pm {}` exit status", args.join(" "));
    }
}
