//! Bulk forms of `pm comment`, `pm hold` and `pm archive` (AGT-1576): each
//! takes several ticket ids — repeated (`pm archive AGT-1 AGT-2`) or
//! comma-separated (`pm hold AGT-1,AGT-2 "why"`) — and lands every
//! ticket's ops in one `commit_batch`, so a call either applies to all of
//! them or to none.
//!
//! Every id is resolved before anything is written: an unknown one fails
//! the whole call (exit `3`) with nothing committed. A ticket named twice
//! is acted on once.
//!
//! Output: one id keeps the verbs' long-standing shape (its display id;
//! `--json` **Ticket**). Several ids print one display id per line, and
//! `--json` prints `{"schema": 1, "results": [{"id", "result", "ticket"}]}`
//! — one entry per distinct ticket, in the order named.
//!
//! `comment` and `hold` also take a trailing text positional (the comment
//! body, the hold reason), so the *last* positional is that text unless
//! `--file` / `--clear` stands in for it. Two guards keep the split from
//! misreading a call: a text that is itself nothing but ticket ids (`pm
//! comment AGT-1 AGT-2`, text forgotten) is refused, and so is a trailing
//! non-id next to `--file` / `--clear` (text given twice).

use serde_json::json;
use ulid::Ulid;

use crate::exit::{CliError, Result};
use crate::verbs::{Ctx, SCHEMA, display_id, find, print_json, ref_id, ticket_json};

/// The ticket ids named on the command line: each positional split on
/// commas and trimmed. An empty piece (`AGT-1,,AGT-2`) is kept so
/// [`resolve`] reports it rather than silently dropping it.
pub(crate) fn split_ids(raw: &[String]) -> Vec<String> {
    raw.iter()
        .flat_map(|r| r.split(','))
        .map(|s| s.trim().to_string())
        .collect()
}

/// Whether `token` is shaped like ticket id(s) — `<PREFIX>-<n>`,
/// `<PREFIX>-?`, or a ULID, optionally comma-separated — regardless of
/// whether any of them exists.
pub(crate) fn looks_like_ids(token: &str) -> bool {
    token.split(',').all(|piece| {
        let p = piece.trim();
        if p.parse::<Ulid>().is_ok() {
            return true;
        }
        match p.rsplit_once('-') {
            Some((prefix, tail)) => {
                !prefix.is_empty()
                    && prefix.bytes().all(|b| b.is_ascii_alphanumeric())
                    && prefix
                        .bytes()
                        .next()
                        .is_some_and(|b| b.is_ascii_alphabetic())
                    && (tail == "?"
                        || (!tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit())))
            }
            None => false,
        }
    })
}

/// Splits `comment`/`hold` positionals into (ids, text). With `text_given`
/// false (no `--file` / `--clear`), the last positional is the text; it
/// must not itself be ticket ids. With `text_given` true, every positional
/// is an id, and a last one that is not id-shaped is `conflict` (exit 2).
/// `missing` is the usage error when the text is needed but absent.
pub(crate) fn split_text<'a>(
    positionals: &'a [String],
    text_given: bool,
    what: &str,
    missing: &str,
    conflict: &str,
) -> Result<(&'a [String], Option<&'a str>)> {
    if text_given {
        if let Some(last) = positionals.last()
            && !looks_like_ids(last)
        {
            return Err(CliError::usage(conflict));
        }
        return Ok((positionals, None));
    }
    match positionals.split_last() {
        Some((text, ids)) if !ids.is_empty() => {
            if looks_like_ids(text) {
                return Err(CliError::usage(format!(
                    "'{}' looks like a ticket id, not {what}: give the {what} after the ids",
                    crate::text::inline(text)
                )));
            }
            Ok((ids, Some(text.as_str())))
        }
        _ => Err(CliError::usage(missing)),
    }
}

/// The tickets `refs` name, resolved before anything is written (any
/// unknown id fails the whole call), deduplicated in first-named order.
/// `many` is whether more than one id was named — it picks the output
/// shape, not the number of distinct tickets.
pub(crate) struct Targets {
    pub tickets: Vec<pm_core::Ticket>,
    pub many: bool,
}

pub(crate) fn resolve(
    store: &pm_store::Store,
    ws: &pm_core::Workspace,
    raw: &[String],
) -> Result<Targets> {
    let refs = split_ids(raw);
    let mut tickets: Vec<pm_core::Ticket> = Vec::new();
    for r in &refs {
        if r.is_empty() {
            return Err(CliError::usage("an empty ticket id in the id list"));
        }
        let t = find(store, ws, r)?;
        if !tickets.iter().any(|seen| seen.id == t.id) {
            tickets.push(t);
        }
    }
    if tickets.is_empty() {
        return Err(CliError::usage("at least one ticket id is required"));
    }
    Ok(Targets {
        tickets,
        many: refs.len() > 1,
    })
}

/// Prints the outcome: per (ticket, result word) as the module docs say.
pub(crate) fn print_results(
    ctx: &Ctx<'_>,
    store: &pm_store::Store,
    ws: &pm_core::Workspace,
    many: bool,
    results: &[(Ulid, &str)],
) -> Result<()> {
    let mut entries = Vec::with_capacity(results.len());
    for (id, result) in results {
        let ticket = store
            .ticket(*id)?
            .ok_or_else(|| CliError::error(format!("ticket {id} vanished after mutation")))?;
        if ctx.json {
            let json = ticket_json(ws, store, &ticket)?;
            if !many {
                print_json(&json);
                return Ok(());
            }
            entries.push(json!({
                "id": ref_id(ws, &ticket),
                "result": result,
                "ticket": json,
            }));
        } else {
            println!("{}", display_id(ws, &ticket));
        }
    }
    if ctx.json {
        print_json(&json!({ "schema": SCHEMA, "results": entries }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_shapes() {
        for yes in [
            "AGT-1",
            "agt-12",
            "PM-?",
            "AGT-1,AGT-2",
            "01M3XC7JQBWKDFW45P00GY03CN",
        ] {
            assert!(looks_like_ids(yes), "{yes}");
        }
        for no in [
            "hi",
            "needs Matt",
            "AGT-",
            "-1",
            "AGT-1 and AGT-2",
            "x, y",
            "",
        ] {
            assert!(!looks_like_ids(no), "{no}");
        }
    }

    #[test]
    fn split_ids_splits_commas_and_keeps_empties() {
        let raw = vec!["A-1,A-2".to_string(), " A-3 ".to_string(), "A-4,".into()];
        assert_eq!(split_ids(&raw), ["A-1", "A-2", "A-3", "A-4", ""]);
    }

    #[test]
    fn split_text_takes_the_last_positional() {
        let p: Vec<String> = ["A-1", "A-2,A-3", "the text"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (ids, text) = split_text(&p, false, "a comment", "m", "c").unwrap();
        assert_eq!(ids, &p[..2]);
        assert_eq!(text, Some("the text"));
        // Text forgotten: the would-be text is an id.
        assert!(split_text(&p[..2], false, "a comment", "m", "c").is_err());
        // Only an id: text missing.
        assert!(split_text(&p[..1], false, "a comment", "m", "c").is_err());
        // --file/--clear: all ids, but a stray text conflicts.
        assert_eq!(split_text(&p[..2], true, "x", "m", "c").unwrap().0.len(), 2);
        assert!(split_text(&p, true, "x", "m", "c").is_err());
    }
}
