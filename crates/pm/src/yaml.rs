//! The one YAML entry point (AGT-1465). `serde-saphyr` replaced
//! `serde_yaml_ng` (which sat on the archived, unsafe `unsafe-libyaml`);
//! its defaults differ from the parser pm's specs and vault files were
//! written against, so every parse goes through [`from_str`], which pins
//! the old behaviour:
//!
//! - only `true`/`false` are booleans (`yes`/`no`/`on`/`off` stay strings);
//! - a duplicate mapping key is an error, not first/last wins;
//! - `<<` is an ordinary key (no merge-key expansion);
//! - errors are the one-line message plus a location, not a rendered
//!   source snippet (callers print `file:line: ...` themselves).
//!
//! Known, accepted differences from `serde_yaml_ng` (checked by running
//! both binaries over a matrix of scalars): an untyped `010` or `1_000`
//! becomes a number (was a string) — quote such values; `.inf`, `.nan` and
//! `1e400` are a parse error (were silently dropped); an out-of-range
//! negative integer becomes a float and an unknown `!tag` a one-key map
//! (both were errors). Error wording differs; the line number is kept.
//!
//! Dynamic values are `serde_json::Value` (saphyr has no `Value` of its
//! own).

use serde::Deserialize;
use serde_saphyr::{DuplicateKeyPolicy, Error, MergeKeyPolicy, Options};

pub(crate) fn options() -> Options {
    let mut o = Options::default();
    o.strict_booleans = true;
    o.duplicate_keys = DuplicateKeyPolicy::Error;
    o.merge_keys = MergeKeyPolicy::AsOrdinary;
    o.with_snippet = false;
    o
}

pub(crate) fn from_str<'de, T: Deserialize<'de>>(input: &'de str) -> Result<T, Error> {
    serde_saphyr::from_str_with_options(input, options())
}

/// The 1-based line of `e`, when it has one.
pub(crate) fn line_of(e: &Error) -> Option<usize> {
    e.location().map(|l| l.line() as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn parse(text: &str) -> Result<Value, Error> {
        from_str(text)
    }

    #[test]
    fn yes_no_on_off_are_strings_and_true_false_booleans() {
        let v = parse("a: yes\nb: No\nc: on\nd: OFF\ne: true\nf: false\ng: y\n").unwrap();
        assert_eq!(
            v,
            json!({"a":"yes","b":"No","c":"on","d":"OFF","e":true,"f":false,"g":"y"})
        );
    }

    #[test]
    fn duplicate_keys_error_and_merge_keys_are_ordinary() {
        assert!(parse("a: 1\na: 2\n").is_err());
        let v = parse("base: &b {x: 1}\nchild:\n  <<: *b\n  y: 2\n").unwrap();
        assert_eq!(v["child"], json!({"<<": {"x": 1}, "y": 2}));
    }

    #[test]
    fn scalars_keep_their_yaml_12_types() {
        let v = parse("n: 7\nf: 1.5\nz: ~\ne:\nd: 2026-09-28\no: 0o14\nh: 0x1F\ns: '1'\n").unwrap();
        assert_eq!(v["n"], 7);
        assert_eq!(v["f"], 1.5);
        assert!(v["z"].is_null() && v["e"].is_null());
        assert_eq!(v["d"], "2026-09-28");
        assert_eq!(v["o"], 12);
        assert_eq!(v["h"], 31);
        assert_eq!(v["s"], "1");
    }

    #[test]
    fn errors_are_one_line_with_a_line_number() {
        let e = parse("a: 1\nb: [x, y\n").unwrap_err();
        assert!(!e.to_string().contains('\n'), "{e}");
        assert!(line_of(&e).is_some());
    }
}
