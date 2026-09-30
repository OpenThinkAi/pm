//! Terminal-control characters in synced text never reach a human-readable
//! output (AGT-1465): ANSI/OSC escapes, C1 controls and bidi overrides are
//! dropped from `show`, `list`, `log`, `holds`, `check`, `ready` and error
//! lines, while `--json` keeps the text exactly as stored.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

/// ESC CSI clear-screen, an OSC window-title set ended by BEL, a C1 CSI, a
/// carriage return, and a right-to-left override.
const EVIL: &str = "\x1b[2Jx\x1b]0;pwned\x07y\u{9b}31mz\rw\u{202e}v";
const CLEAN: &str = "[2Jx]0;pwnedy31mzwv";

struct Sandbox {
    home: TempDir,
    ws: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let ws = home.path().join("ws");
        let sb = Sandbox { home, ws };
        let out = sb.pm(&["init", "--prefix", "AGT", "--preset", "saltline"]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        sb
    }

    fn pm(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pm"))
            .args(args)
            .arg("--workspace")
            .arg(&self.ws)
            .env_clear()
            .env("HOME", self.home.path())
            .env("USER", "tester")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn no_controls(s: &str) {
    for c in s.chars() {
        let bidi = ('\u{202a}'..='\u{202e}').contains(&c);
        let control = c.is_control() && c != '\n' && c != '\t';
        assert!(!bidi && !control, "terminal control {c:?} in {s:?}");
    }
}

#[test]
fn text_output_drops_terminal_controls_and_json_keeps_them() {
    let sb = Sandbox::new();
    let title = format!("T {EVIL}");
    assert!(sb.pm(&["new", "--title", &title]).status.success());
    let body = format!("line one\n{EVIL}\n\tindented");
    assert!(sb.pm(&["comment", "AGT-1", &body]).status.success());
    assert!(sb.pm(&["hold", "AGT-1", EVIL]).status.success());

    for args in [
        vec!["show", "AGT-1"],
        vec!["list"],
        vec!["log", "AGT-1"],
        vec!["holds"],
        vec!["check"],
        vec!["ready"],
        vec!["show", "AGT-1", "--field", "title"],
    ] {
        let out = sb.pm(&args);
        let stdout = text(&out.stdout);
        no_controls(&stdout);
        no_controls(&text(&out.stderr));
        if args[0] == "show" || args[0] == "holds" {
            assert!(stdout.contains(CLEAN), "{args:?}: {stdout}");
        }
    }

    // Multi-line output keeps its newlines and tabs.
    let shown = text(&sb.pm(&["show", "AGT-1"]).stdout);
    assert!(shown.contains("  line one\n"), "{shown}");
    assert!(shown.contains("  \tindented"), "{shown}");

    // A title cannot fake an extra row: newlines flatten in list rows.
    assert!(sb.pm(&["new", "--title", "a\nfake row"]).status.success());
    let list = text(&sb.pm(&["list"]).stdout);
    assert_eq!(list.lines().count(), 2, "{list}");

    // --json is unchanged (the store's text, JSON-escaped).
    let json = text(&sb.pm(&["show", "AGT-1", "--json"]).stdout);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["title"], title.as_str());
    assert_eq!(v["comments"][0]["body"], body.as_str());
}

#[test]
fn error_lines_are_sanitised() {
    let sb = Sandbox::new();
    let out = sb.pm(&["show", &format!("NOPE{EVIL}")]);
    assert!(!out.status.success());
    let stderr = text(&out.stderr);
    no_controls(&stderr);
    assert!(stderr.contains("NOPE"), "{stderr}");
}
