//! Spike O1 (AGT-1338): measure Loro vs yrs on the workload pm actually has —
//! a ~2 KB markdown ticket body edited 50 times, one `body.edit` op per edit.
//!
//! Run with `cargo run --release -p pm-core --example text_crdt_bench`.
//! Every number in `projects/pm/research/text-crdt-spike.md` (vault) comes
//! from this program; re-run it rather than editing the table by hand.
//!
//! Both libraries see the *same* deterministic edit script (seeded LCG) so
//! the update sizes are directly comparable. The text is ASCII-only so byte,
//! UTF-16 and code-point indices coincide and neither library is disadvantaged
//! by index conversion.

use std::fmt::Write as _;
use std::time::Instant;

use loro::{ExportMode, LoroDoc, UpdateOptions};
use yrs::updates::decoder::Decode;
use yrs::{Doc, GetString, ReadTxn, StateVector, Text, Transact, Update};

const TARGET_DOC_BYTES: usize = 2048;
const EDITS: usize = 50;
const SEED: u64 = 0x5EED_1338;

/// Tiny deterministic PRNG (LCG); no rand dependency in a spike harness.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// One edit, expressed against the plain-text model so both CRDTs apply the
/// identical operation. `delete` chars at `pos`, then insert `insert` there.
#[derive(Debug, Clone)]
struct Edit {
    pos: usize,
    delete: usize,
    insert: String,
}

impl Edit {
    fn apply_to(&self, model: &mut String) {
        model.replace_range(self.pos..self.pos + self.delete, &self.insert);
    }
}

fn build_doc() -> String {
    let sentences = [
        "The importer must keep ticket ids stable across re-runs.",
        "Blockers are an OR-set; adds win over concurrent removes.",
        "Claims are arbitrated by the authority, never merged.",
        "Exit code 75 means the ticket was taken by another agent.",
        "Materialized tables are rebuilt by replaying the op log.",
        "Every op carries an HLC and the actor id as a tie-break.",
    ];
    let mut doc = String::from("# Spike O1: text CRDT for ticket bodies\n\n");
    let mut i = 0;
    while doc.len() < TARGET_DOC_BYTES {
        if i % 4 == 0 {
            let _ = writeln!(doc, "\n## Section {}\n", i / 4 + 1);
        }
        if i % 4 == 3 {
            let _ = writeln!(doc, "- {}", sentences[i % sentences.len()]);
        } else {
            let _ = writeln!(
                doc,
                "{} {}",
                sentences[i % sentences.len()],
                sentences[(i + 1) % sentences.len()]
            );
        }
        i += 1;
    }
    doc.truncate(TARGET_DOC_BYTES);
    // Never cut inside a word so the edit script stays readable.
    if let Some(cut) = doc.rfind(' ') {
        doc.truncate(cut);
    }
    assert!(doc.is_ascii(), "harness text must be ASCII (index parity)");
    doc
}

fn build_edits(model_start: &str, rng: &mut Lcg) -> Vec<Edit> {
    let words = [
        "merge", "offline", "hub", "ticket", "replica", "op-log", "HLC", "actor",
    ];
    let mut model = model_start.to_string();
    let mut edits = Vec::with_capacity(EDITS);
    for _ in 0..EDITS {
        // Land on a space so inserts read like typing a word.
        let spaces: Vec<usize> = model.match_indices(' ').map(|(i, _)| i).collect();
        let pos = spaces[rng.below(spaces.len())];
        let edit = match rng.below(4) {
            0 | 1 => Edit {
                pos,
                delete: 0,
                insert: format!(" {}", words[rng.below(words.len())]),
            },
            2 => {
                let max = (model.len() - pos).min(12);
                Edit {
                    pos,
                    delete: 1 + rng.below(max.max(1)),
                    insert: String::new(),
                }
            }
            _ => {
                let max = (model.len() - pos).min(8);
                Edit {
                    pos,
                    delete: 1 + rng.below(max.max(1)),
                    insert: format!(" {}", words[rng.below(words.len())]),
                }
            }
        };
        edit.apply_to(&mut model);
        edits.push(edit);
    }
    edits
}

#[derive(Default)]
struct Sizes(Vec<usize>);

impl Sizes {
    fn push(&mut self, n: usize) {
        self.0.push(n);
    }

    fn row(&self) -> String {
        let mut v = self.0.clone();
        v.sort_unstable();
        let total: usize = v.iter().sum();
        let mean = total as f64 / v.len() as f64;
        let median = v[v.len() / 2];
        format!(
            "total {total} B | min {} | median {median} | mean {mean:.1} | max {}",
            v[0],
            v[v.len() - 1]
        )
    }
}

/// Common-prefix/suffix diff on char boundaries — what `Body::diff_from_text`
/// has to do by hand for a library without a built-in text diff.
fn prefix_suffix_diff(old: &str, new: &str) -> Edit {
    let prefix = old
        .char_indices()
        .zip(new.chars())
        .take_while(|((_, a), b)| a == b)
        .map(|((i, a), _)| i + a.len_utf8())
        .last()
        .unwrap_or(0);
    let old_rest = &old[prefix..];
    let new_rest = &new[prefix..];
    let suffix = old_rest
        .chars()
        .rev()
        .zip(new_rest.chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum::<usize>();
    Edit {
        pos: prefix,
        delete: old_rest.len() - suffix,
        insert: new_rest[..new_rest.len() - suffix].to_string(),
    }
}

struct Report {
    name: String,
    empty_update: usize,
    init_update: usize,
    per_edit: Sizes,
    per_edit_alt: Option<Sizes>,
    batched: usize,
    snapshot: usize,
    editor_save_diff: usize,
    editor_save_naive: usize,
    elapsed_ms: f64,
    replica_ok: bool,
    concurrent_ok: bool,
}

impl Report {
    fn print(&self) {
        println!("### {}", self.name);
        println!(
            "- empty update (fixed framing overhead): {} B",
            self.empty_update
        );
        println!("- initial 2 KB insert update: {} B", self.init_update);
        println!("- per-edit updates (50): {}", self.per_edit.row());
        if let Some(alt) = &self.per_edit_alt {
            println!("- per-edit updates, v2 encoding: {}", alt.row());
        }
        println!(
            "- one batched update covering all 50 edits: {} B",
            self.batched
        );
        println!("- full snapshot after 50 edits: {} B", self.snapshot);
        println!(
            "- $EDITOR save (one word changed): diff update {} B vs naive replace-all {} B",
            self.editor_save_diff, self.editor_save_naive
        );
        println!("- 50 edits + encode wall time: {:.2} ms", self.elapsed_ms);
        println!(
            "- fresh replica replaying the 51 updates matches: {}",
            self.replica_ok
        );
        println!(
            "- two replicas editing concurrently then exchanging converge: {}",
            self.concurrent_ok
        );
        println!();
    }
}

fn bench_loro(
    doc0: &str,
    edits: &[Edit],
    final_text: &str,
    saved: &str,
) -> Result<Report, Box<dyn std::error::Error>> {
    let doc = LoroDoc::new();
    doc.set_peer_id(1)?;
    let text = doc.get_text("body");
    let mut updates = Vec::new();

    let empty_update = doc.export(ExportMode::updates(&doc.oplog_vv()))?.len();
    let vv = doc.oplog_vv();
    text.insert(0, doc0)?;
    doc.commit();
    let init = doc.export(ExportMode::updates(&vv))?;
    let init_update = init.len();
    updates.push(init);
    let vv_after_init = doc.oplog_vv();

    let started = Instant::now();
    let mut per_edit = Sizes::default();
    for e in edits {
        let before = doc.oplog_vv();
        if e.delete > 0 {
            text.delete(e.pos, e.delete)?;
        }
        if !e.insert.is_empty() {
            text.insert(e.pos, &e.insert)?;
        }
        doc.commit();
        let bytes = doc.export(ExportMode::updates(&before))?;
        per_edit.push(bytes.len());
        updates.push(bytes);
    }
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(
        text.to_string(),
        final_text,
        "loro text diverged from model"
    );

    let batched = doc.export(ExportMode::updates(&vv_after_init))?.len();
    let snapshot = doc.export(ExportMode::Snapshot)?.len();

    // $EDITOR save: whole document handed back with one word changed.
    let before = doc.oplog_vv();
    text.update(saved, UpdateOptions::default())?;
    doc.commit();
    let editor_save_diff = doc.export(ExportMode::updates(&before))?.len();
    assert_eq!(text.to_string(), saved);
    let naive = LoroDoc::new();
    naive.set_peer_id(1)?;
    let ntext = naive.get_text("body");
    ntext.insert(0, final_text)?;
    naive.commit();
    let before = naive.oplog_vv();
    ntext.delete(0, ntext.len_unicode())?;
    ntext.insert(0, saved)?;
    naive.commit();
    let editor_save_naive = naive.export(ExportMode::updates(&before))?.len();

    // Replica replays the per-edit updates.
    let replica = LoroDoc::new();
    replica.set_peer_id(2)?;
    for u in &updates {
        replica.import(u)?;
    }
    let replica_ok = replica.get_text("body").to_string() == final_text;

    // Concurrent edits on two replicas, then exchange.
    let b = LoroDoc::new();
    b.set_peer_id(3)?;
    b.import(&replica.export(ExportMode::Snapshot)?)?;
    let vv_a = replica.oplog_vv();
    let vv_b = b.oplog_vv();
    replica.get_text("body").insert(0, "A: ")?;
    replica.commit();
    let len_b = b.get_text("body").len_unicode();
    b.get_text("body").insert(len_b, "\nB was here")?;
    b.commit();
    let from_a = replica.export(ExportMode::updates(&vv_a))?;
    let from_b = b.export(ExportMode::updates(&vv_b))?;
    replica.import(&from_b)?;
    b.import(&from_a)?;
    let ta = replica.get_text("body").to_string();
    let tb = b.get_text("body").to_string();
    let concurrent_ok = ta == tb && ta.starts_with("A: ") && ta.ends_with("\nB was here");

    Ok(Report {
        name: format!("loro {}", loro::LORO_VERSION),
        empty_update,
        init_update,
        per_edit,
        per_edit_alt: None,
        batched,
        snapshot,
        editor_save_diff,
        editor_save_naive,
        elapsed_ms,
        replica_ok,
        concurrent_ok,
    })
}

fn bench_yrs(
    doc0: &str,
    edits: &[Edit],
    final_text: &str,
    saved: &str,
) -> Result<Report, Box<dyn std::error::Error>> {
    let doc = Doc::with_client_id(1);
    let text = doc.get_or_insert_text("body");
    let mut updates = Vec::new();

    let empty_update = doc.transact_mut().encode_update_v1().len();
    let init = {
        let mut txn = doc.transact_mut();
        text.insert(&mut txn, 0, doc0);
        txn.encode_update_v1()
    };
    let init_update = init.len();
    updates.push(init);
    let sv_after_init = doc.transact().state_vector();

    let started = Instant::now();
    let mut per_edit = Sizes::default();
    let mut per_edit_v2 = Sizes::default();
    for e in edits {
        let mut txn = doc.transact_mut();
        if e.delete > 0 {
            text.remove_range(&mut txn, e.pos as u32, e.delete as u32);
        }
        if !e.insert.is_empty() {
            text.insert(&mut txn, e.pos as u32, &e.insert);
        }
        let v1 = txn.encode_update_v1();
        per_edit_v2.push(txn.encode_update_v2().len());
        per_edit.push(v1.len());
        updates.push(v1);
    }
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(
        text.get_string(&doc.transact()),
        final_text,
        "yrs text diverged from model"
    );

    let batched = doc.transact().encode_diff_v1(&sv_after_init).len();
    let snapshot = doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default())
        .len();

    // $EDITOR save: yrs has no built-in text diff, so do prefix/suffix by hand.
    let editor_save_diff = {
        let d = prefix_suffix_diff(final_text, saved);
        let mut txn = doc.transact_mut();
        if d.delete > 0 {
            text.remove_range(&mut txn, d.pos as u32, d.delete as u32);
        }
        text.insert(&mut txn, d.pos as u32, &d.insert);
        txn.encode_update_v1().len()
    };
    assert_eq!(text.get_string(&doc.transact()), saved);
    let editor_save_naive = {
        let naive = Doc::with_client_id(1);
        let t = naive.get_or_insert_text("body");
        {
            let mut txn = naive.transact_mut();
            t.insert(&mut txn, 0, final_text);
        }
        let mut txn = naive.transact_mut();
        let len = t.len(&txn);
        t.remove_range(&mut txn, 0, len);
        t.insert(&mut txn, 0, saved);
        txn.encode_update_v1().len()
    };

    let replica = Doc::with_client_id(2);
    let rtext = replica.get_or_insert_text("body");
    for u in &updates {
        replica.transact_mut().apply_update(Update::decode_v1(u)?)?;
    }
    let replica_ok = rtext.get_string(&replica.transact()) == final_text;

    let b = Doc::with_client_id(3);
    let btext = b.get_or_insert_text("body");
    {
        let full = replica
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        b.transact_mut().apply_update(Update::decode_v1(&full)?)?;
    }
    let sv_a = replica.transact().state_vector();
    let sv_b = b.transact().state_vector();
    rtext.insert(&mut replica.transact_mut(), 0, "A: ");
    {
        let mut txn = b.transact_mut();
        let len = btext.len(&txn);
        btext.insert(&mut txn, len, "\nB was here");
    }
    let from_a = replica.transact().encode_diff_v1(&sv_a);
    let from_b = b.transact().encode_diff_v1(&sv_b);
    replica
        .transact_mut()
        .apply_update(Update::decode_v1(&from_b)?)?;
    b.transact_mut().apply_update(Update::decode_v1(&from_a)?)?;
    let ta = rtext.get_string(&replica.transact());
    let tb = btext.get_string(&b.transact());
    let concurrent_ok = ta == tb && ta.starts_with("A: ") && ta.ends_with("\nB was here");

    Ok(Report {
        name: format!("yrs {}", yrs_version()),
        empty_update,
        init_update,
        per_edit,
        per_edit_alt: Some(per_edit_v2),
        batched,
        snapshot,
        editor_save_diff,
        editor_save_naive,
        elapsed_ms,
        replica_ok,
        concurrent_ok,
    })
}

/// yrs exposes no version constant; read it from the manifest cargo resolved.
fn yrs_version() -> &'static str {
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .lines()
        .find_map(|l| l.trim().strip_prefix("yrs = \"")?.strip_suffix('"'))
        .unwrap_or("?")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let doc0 = build_doc();
    let mut rng = Lcg(SEED);
    let edits = build_edits(&doc0, &mut rng);
    let mut model = doc0.clone();
    for e in &edits {
        e.apply_to(&mut model);
    }
    let final_text = model;
    // $EDITOR scenario: the whole body comes back with one word changed.
    let saved = final_text.replacen("ticket", "issue", 1);
    assert_ne!(saved, final_text);

    let inserted: usize = edits.iter().map(|e| e.insert.len()).sum();
    let deleted: usize = edits.iter().map(|e| e.delete).sum();
    println!("## text_crdt_bench (seed {SEED:#x})\n");
    println!(
        "- document: {} B initial, {} B final; {} edits ({} B inserted, {} B deleted)",
        doc0.len(),
        final_text.len(),
        edits.len(),
        inserted,
        deleted
    );
    println!(
        "- profile: {}\n",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );

    bench_loro(&doc0, &edits, &final_text, &saved)?.print();
    bench_yrs(&doc0, &edits, &final_text, &saved)?.print();
    Ok(())
}
