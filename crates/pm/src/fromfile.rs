//! `--from-file <path|->` (AGT-1480): non-interactive writes of a ticket
//! description (`pm edit <id> --from-file`) and, in `crate::project`, a
//! project document. The text goes through the same line-faithful
//! `Body::diff_from_text` (`edit::body_update`) the `$EDITOR` flows use,
//! minted by a fresh per-invocation peer, so a concurrent edit merges
//! instead of being overwritten. Nothing is launched and nothing prompts.

use std::io::Read;

use anyhow::Context;
use pm_core::Payload;
use pm_core::op::BodyEdit;
use ulid::Ulid;

use crate::edit;
use crate::exit::{CliError, Result};
use crate::verbs::{Ctx, Stamper, display_id, find, print_json, ticket_json};

/// The text named by `--from-file`: a path, or `-` for stdin. Must be UTF-8.
pub(crate) fn read_source(src: &str) -> Result<String> {
    if src == "-" {
        let mut bytes = Vec::new();
        std::io::stdin()
            .read_to_end(&mut bytes)
            .context("reading stdin")?;
        return String::from_utf8(bytes)
            .map_err(|_| CliError::usage("--from-file: stdin is not valid UTF-8"));
    }
    let bytes = std::fs::read(src).with_context(|| format!("reading {src}"))?;
    String::from_utf8(bytes)
        .map_err(|_| CliError::usage(format!("--from-file: {src} is not valid UTF-8")))
}

/// `pm edit <id> --from-file <path|->`: replaces the description only (the
/// frontmatter fields are `pm set`'s). The file's text is taken verbatim.
/// `<id>` is `AGT-n` or, for a ticket still awaiting its hub number, its
/// ULID. Unchanged text commits nothing.
pub(crate) fn ticket_description(ctx: &Ctx<'_>, reference: &str, src: &str) -> Result<()> {
    let actor = ctx.actor()?;
    let (mut store, ws) = ctx.open()?;
    let ticket = find(&store, &ws, reference)?;
    let shown = display_id(&ws, &ticket);
    let view = store
        .ticket_view(ticket.id)?
        .ok_or_else(|| CliError::not_found(format!("no ticket {shown}")))?;
    let text = read_source(src)?;
    if text == view.snapshot().description {
        eprintln!("pm: {shown}: no changes");
    } else {
        let update = edit::body_update(&view.body, &text, edit::session_peer(Ulid::new()))?;
        let mut stamper = Stamper::new(&store, actor)?;
        let op = stamper.op(ticket.id, Payload::BodyEdit(BodyEdit { update }));
        store.commit_batch(&[op], &[])?;
    }
    let ticket = store
        .ticket(ticket.id)?
        .ok_or_else(|| CliError::error(format!("ticket {shown} vanished after edit")))?;
    if ctx.json {
        print_json(&ticket_json(&ws, &store, &ticket)?);
    } else {
        println!("{shown}");
    }
    Ok(())
}
