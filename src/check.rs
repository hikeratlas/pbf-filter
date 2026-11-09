//! Whether a filtered PBF still serves a (possibly changed) set of profiles,
//! so a build can use it instead of the whole PBF.
//!
//! A filtered PBF goes stale only if the profiles now read something it may
//! not have:
//! - ways or nodes: their `way_keys` or `node_keys` match more than the ones
//!   it was made for (recorded in its header), or a `node_function` with no
//!   `node_keys` stops being empty;
//! - relations: one they'd now read (its tags match `way_keys`, or, if not a
//!   multipolygon, `relation_scan_function` accepts it) isn't in it. Every
//!   relation's tags are in a sidecar (`relations`), made from the same PBF,
//!   for running the new scan functions on.
//!
//! Changes to way_function, node_function or relation_function, or a scan
//! function that accepts fewer relations, never make it stale. A relation
//! it has has its member ways, and (unless a multipolygon) member nodes,
//! so a profile can come to accept one it kept for its tags.

use crate::filter::{sections, tag_bytes};
use crate::header;
use crate::pbf::{self, Block};
use crate::profile::{Profile, Rules};
use crate::write::{BlockBuilder, Compression, frame};
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Write};

fn open(path: &str) -> Result<(memmap2::Mmap, Vec<pbf::BlobRef>), String> {
    let file = File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let map = unsafe { memmap2::Mmap::map(&file) }.map_err(|e| format!("{path}: {e}"))?;
    let blobs = pbf::index(&map)?;
    Ok((map, blobs))
}

/// The sidecar: every relation of a sorted PBF, with its tags and no
/// members, and the source's size in its header.
pub fn relations(input: &str, output: &str, c: Compression) -> Result<u64, String> {
    let (map, blobs) = open(input)?;
    let data: &[u8] = &map;
    let (head, _, _, first_rel) = sections(input, data, &blobs)?;
    let mut out = BufWriter::new(File::create(output).map_err(|e| format!("{output}: {e}"))?);
    let record = serde_json::json!({ "source_bytes": data.len(), "relations": true });
    out.write_all(&frame("OSMHeader", &header::header_block(Some(&head), true, Some(&record)), c)).map_err(|e| e.to_string())?;
    let (mut b, mut n) = (BlockBuilder::new(), 0u64);
    for blob in &blobs[first_rel..] {
        let raw = pbf::read_blob(data, blob)?;
        let block = Block::parse(&raw);
        block.relations(|r| {
            b.add_relation(r.id, tag_bytes(&block, &r.tags), std::iter::empty());
            n += 1;
            if b.is_full() {
                out.write_all(&frame("OSMData", &b.finish(), c)).unwrap();
            }
        });
    }
    if b.len() > 0 {
        out.write_all(&frame("OSMData", &b.finish(), c)).map_err(|e| e.to_string())?;
    }
    out.flush().map_err(|e| e.to_string())?;
    Ok(n)
}

pub enum Verdict {
    Fresh { relations: u64 },
    Stale(String),
}

/// Whether FILTERED (made by `filter`) still serves PROFILES, using the
/// sidecar RELATIONS (made by `relations` from the same PBF).
pub fn check(filtered: &str, relations: &str, profiles: &[Profile]) -> Result<Verdict, String> {
    let (fmap, fblobs) = open(filtered)?;
    let (fhead, _, _, ffirst_rel) = sections(filtered, &fmap, &fblobs)?;
    let rec = header::record(&fhead).ok_or_else(|| format!("{filtered} has no pbf-filter record; was it made by `pbf-filter filter`?"))?;
    let old: Vec<Rules> = rec["profiles"].as_array().ok_or("no profiles in the record")?.iter().map(Rules::from_json).collect::<Result<_, _>>()?;
    let (rmap, rblobs) = open(relations)?;
    let (rhead, _, _, rfirst_rel) = sections(relations, &rmap, &rblobs)?;
    let rrec = header::record(&rhead)
        .filter(|r| r["relations"] == true)
        .ok_or_else(|| format!("{relations} isn't a relations sidecar (made by `pbf-filter relations`)"))?;
    if rrec["source_bytes"] != rec["source_bytes"] {
        return Err(format!(
            "{filtered} and {relations} were made from different PBFs ({} and {} bytes)",
            rec["source_bytes"], rrec["source_bytes"]
        ));
    }

    // Ways and nodes: the keys.
    for p in profiles {
        if let Some(why) = p.rules.uncovered_by(&old) {
            return Ok(Verdict::Stale(why));
        }
    }

    // Relations: what the profiles read now must be in the filtered PBF.
    let mut kept: HashSet<i64> = HashSet::new();
    for blob in &fblobs[ffirst_rel..] {
        let raw = pbf::read_blob(&fmap, blob)?;
        Block::parse(&raw).relations(|r| {
            kept.insert(r.id);
        });
    }
    let mut n = 0u64;
    for blob in &rblobs[rfirst_rel..] {
        let raw = pbf::read_blob(&rmap, blob)?;
        let block = Block::parse(&raw);
        let mut verdict: Result<Option<String>, String> = Ok(None);
        block.relations(|r| {
            n += 1;
            if !matches!(verdict, Ok(None)) || kept.contains(&r.id) {
                return;
            }
            let tags = tag_bytes(&block, &r.tags);
            let multipolygon = tags.iter().any(|&(k, v)| k == b"type" && v == b"multipolygon");
            for p in profiles {
                if p.rules.keeps_way(tags.iter().copied()) {
                    verdict = Ok(Some(format!("{}: way_keys match relation {}, which the PBF doesn't have", p.rules.name, r.id)));
                    return;
                }
                if !multipolygon {
                    match p.accepts(r.id, &tags) {
                        Ok(true) => {
                            verdict = Ok(Some(format!(
                                "{}: relation_scan_function accepts relation {}, which the PBF doesn't have",
                                p.rules.name, r.id
                            )));
                            return;
                        }
                        Ok(false) => {}
                        Err(e) => {
                            verdict = Err(e);
                            return;
                        }
                    }
                }
            }
        });
        if let Some(why) = verdict? {
            return Ok(Verdict::Stale(why));
        }
    }
    Ok(Verdict::Fresh { relations: n })
}
