//! `recompress`: a PBF's data, re-encoded without metadata (versions,
//! timestamps, changesets, users: tilemaker reads none of it) and with zstd
//! (or zlib) blocks. Each block is re-encoded on its own, in parallel, so
//! the file keeps its blocks and order.
//!
//! `merge`: several sorted PBFs (e.g. Geofabrik's extracts of neighbouring
//! countries) as one sorted PBF, with one copy of what's in more than one
//! (the objects along their border, the same in each). Filtered PBFs (or
//! relations sidecars) of neighbouring extracts merge into what filtering
//! (or `relations` on) the merged extract gives, record included.

use crate::header;
use crate::pbf::{self, Block, BlobRef, Kind, Member};
use crate::write::{BLOCK_SIZE, BlockBuilder, Compression, frame};
use rayon::prelude::*;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, sync_channel};

/// Blocks decoded or compressed at a time, across the cores.
const WINDOW: usize = 256;

fn open(path: &str) -> Result<(memmap2::Mmap, Vec<BlobRef>), String> {
    let file = File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let map = unsafe { memmap2::Mmap::map(&file) }.map_err(|e| format!("{path}: {e}"))?;
    let blobs = pbf::index(&map)?;
    Ok((map, blobs))
}

fn header_of(data: &[u8], blobs: &[BlobRef]) -> Result<Vec<u8>, String> {
    let b = blobs.iter().find(|b| b.is_header).ok_or("no OSMHeader block")?;
    pbf::read_blob(data, b)
}

/// One block's entities, re-encoded into `b`.
fn copy_block(block: &Block, b: &mut BlockBuilder) {
    let tags = |t: &[(u32, u32)]| t.iter().map(|&(k, v)| (block.string(k), block.string(v))).collect::<Vec<_>>();
    block.nodes(|n| b.add_node(n.id, n.lat, n.lon, tags(&n.tags)));
    block.ways(|w| b.add_way(w.id, tags(&w.tags), &w.refs));
    block.relations(|r| b.add_relation(r.id, tags(&r.tags), r.members.iter().map(|&(t, id, role)| (t, id, block.string(role)))));
}

pub fn recompress(input: &str, output: &str, c: Compression) -> Result<(), String> {
    let (map, blobs) = open(input)?;
    let data: &[u8] = &map;
    let head = header_of(data, &blobs)?;
    let mut out = BufWriter::new(File::create(output).map_err(|e| format!("{output}: {e}"))?);
    out.write_all(&frame("OSMHeader", &header::header_block(Some(&head), header::is_sorted(&head), None), c)).map_err(|e| e.to_string())?;
    let data_blobs: Vec<&BlobRef> = blobs.iter().filter(|b| !b.is_header).collect();
    for window in data_blobs.chunks(WINDOW) {
        let frames: Vec<Result<Vec<u8>, String>> = window
            .par_iter()
            .map(|blob| {
                let raw = pbf::read_blob(data, blob)?;
                let block = Block::parse(&raw);
                let mut b = BlockBuilder::new();
                copy_block(&block, &mut b);
                Ok(frame("OSMData", &b.finish(), c))
            })
            .collect();
        for f in frames {
            out.write_all(&f?).map_err(|e| e.to_string())?;
        }
    }
    out.flush().map_err(|e| e.to_string())
}

// ---- merge -----------------------------------------------------------------

/// A block's string table, shared by the entities decoded from it, so an
/// entity's tags are indices rather than strings of their own: decoding
/// allocates per block, not per tag.
type Strings = Arc<Vec<Box<[u8]>>>;

/// An entity, owned, for merging across files.
enum Entity {
    Node { id: i64, lat: i64, lon: i64, tags: Vec<(u32, u32)>, s: Strings },
    Way { id: i64, tags: Vec<(u32, u32)>, refs: Vec<i64>, s: Strings },
    Relation { id: i64, tags: Vec<(u32, u32)>, members: Vec<(Member, i64, u32)>, s: Strings },
}

fn string(s: &Strings, i: u32) -> &[u8] {
    s.get(i as usize).map_or(&[][..], |b| &b[..])
}

impl Entity {
    fn key(&self) -> (Kind, i64) {
        match self {
            Entity::Node { id, .. } => (Kind::Nodes, *id),
            Entity::Way { id, .. } => (Kind::Ways, *id),
            Entity::Relation { id, .. } => (Kind::Relations, *id),
        }
    }
    fn add_to(&self, b: &mut BlockBuilder) {
        fn t<'a>(s: &'a Strings, tags: &'a [(u32, u32)]) -> impl Iterator<Item = (&'a [u8], &'a [u8])> + 'a {
            tags.iter().map(move |&(k, v)| (string(s, k), string(s, v)))
        }
        match self {
            Entity::Node { id, lat, lon, tags, s } => b.add_node(*id, *lat, *lon, t(s, tags)),
            Entity::Way { id, tags, refs, s } => b.add_way(*id, t(s, tags), refs),
            Entity::Relation { id, tags, members, s } => {
                b.add_relation(*id, t(s, tags), members.iter().map(|&(m, id, role)| (m, id, string(s, role))))
            }
        }
    }
}

fn entities(block: &Block) -> Vec<Entity> {
    let s: Strings = Arc::new(block.strings.iter().map(|x| Box::<[u8]>::from(*x)).collect());
    let mut out = vec![];
    block.nodes(|n| out.push(Entity::Node { id: n.id, lat: n.lat, lon: n.lon, tags: n.tags, s: s.clone() }));
    block.ways(|w| out.push(Entity::Way { id: w.id, tags: w.tags, refs: w.refs, s: s.clone() }));
    block.relations(|r| out.push(Entity::Relation { id: r.id, tags: r.tags, members: r.members, s: s.clone() }));
    out
}

/// A file's entities in order, decoded a window of blocks at a time (in
/// parallel) on a thread of its own.
fn reader(path: String) -> Result<(Vec<u8>, Receiver<Result<Vec<Entity>, String>>), String> {
    let (map, blobs) = open(&path)?;
    let head = header_of(&map, &blobs)?;
    if !header::is_sorted(&head) {
        return Err(format!("{path} isn't marked sorted (Sort.Type_then_ID)"));
    }
    let (tx, rx) = sync_channel(4);
    std::thread::spawn(move || {
        let data: &[u8] = &map;
        let data_blobs: Vec<&BlobRef> = blobs.iter().filter(|b| !b.is_header).collect();
        for window in data_blobs.chunks(WINDOW) {
            let decoded: Vec<Result<Vec<Entity>, String>> = window
                .par_iter()
                .map(|blob| pbf::read_blob(data, blob).map(|raw| entities(&Block::parse(&raw))))
                .collect();
            for d in decoded {
                if tx.send(d).is_err() {
                    return;
                }
            }
        }
    });
    Ok((head, rx))
}

struct Cursor {
    rx: Receiver<Result<Vec<Entity>, String>>,
    buf: std::vec::IntoIter<Entity>,
    head: Option<Entity>,
}

impl Cursor {
    /// Moves to the next entity; false at the end.
    fn advance(&mut self) -> Result<bool, String> {
        loop {
            if let Some(e) = self.buf.next() {
                self.head = Some(e);
                return Ok(true);
            }
            match self.rx.recv() {
                Ok(block) => self.buf = block?.into_iter(),
                Err(_) => {
                    self.head = None;
                    return Ok(false);
                }
            }
        }
    }
}

/// The record for a merge of files with these records: none if none has
/// one; else they must all be filtered PBFs made with the same rules, or all
/// relations sidecars, and the sources' sizes add up (so a merge of filtered
/// PBFs and a merge of their sidecars still match in `check`).
fn merged_record(inputs: &[String], records: &[Option<serde_json::Value>]) -> Result<Option<serde_json::Value>, String> {
    if records.iter().all(Option::is_none) {
        return Ok(None);
    }
    let mut all = vec![];
    for (path, r) in inputs.iter().zip(records) {
        all.push(r.as_ref().ok_or_else(|| format!("{path} has no pbf-filter record, but other inputs do"))?);
    }
    let mut source_bytes = 0u64;
    for (path, r) in inputs.iter().zip(&all) {
        source_bytes += r["source_bytes"].as_u64().ok_or_else(|| format!("{path}: no source_bytes in its record"))?;
    }
    let first = all[0];
    if all.iter().all(|r| r["relations"] == true) {
        return Ok(Some(serde_json::json!({ "source_bytes": source_bytes, "relations": true })));
    }
    for (path, r) in inputs.iter().zip(&all) {
        if r["relations"] == true || r["profiles"].is_null() {
            return Err(format!("{path} isn't the same kind of file as {} (filtered PBF or relations sidecar)", inputs[0]));
        }
        if r["profiles"] != first["profiles"] {
            return Err(format!("{path} was filtered with different profiles from {}", inputs[0]));
        }
    }
    Ok(Some(serde_json::json!({ "source_bytes": source_bytes, "profiles": first["profiles"] })))
}

pub fn merge(inputs: &[String], output: &str, c: Compression) -> Result<(), String> {
    let mut cursors = vec![];
    let mut records = vec![];
    for path in inputs {
        let (head, rx) = reader(path.clone())?;
        records.push(header::record(&head));
        let mut cur = Cursor { rx, buf: vec![].into_iter(), head: None };
        cur.advance()?;
        cursors.push(cur);
    }
    let record = merged_record(inputs, &records)?;
    let mut out = BufWriter::new(File::create(output).map_err(|e| format!("{output}: {e}"))?);
    out.write_all(&frame("OSMHeader", &header::header_block(None, true, record.as_ref()), c)).map_err(|e| e.to_string())?;
    let c = c;
    // The merge itself only orders the entities and drops duplicates, into
    // batches of a block's worth (a new one at each change of kind, as a
    // builder would start a new block); encoding and compressing each batch
    // is the costly part, done a window of batches at a time in parallel.
    let (mut batch, mut kind, mut batches): (Vec<Entity>, _, Vec<Vec<Entity>>) = (Vec::with_capacity(BLOCK_SIZE), None, vec![]);
    // Encoding and writing run on a thread of their own, so the merge
    // doesn't wait for them (nor, through it, the readers).
    let (tx, rx) = sync_channel::<Vec<Vec<Entity>>>(2);
    let writer = std::thread::spawn(move || -> Result<(), String> {
        for window in rx {
            let frames: Vec<Vec<u8>> = window
                .par_iter()
                .map(|entities| {
                    let mut b = BlockBuilder::new();
                    for e in entities {
                        e.add_to(&mut b);
                    }
                    frame("OSMData", &b.finish(), c)
                })
                .collect();
            drop(window);
            for f in frames {
                out.write_all(&f).map_err(|e| e.to_string())?;
            }
        }
        out.flush().map_err(|e| e.to_string())
    });
    let flush = |batches: &mut Vec<Vec<Entity>>| -> Result<(), String> {
        tx.send(std::mem::take(batches)).map_err(|_| "the writer stopped".to_string())
    };
    loop {
        // The smallest (kind, id) at the head of any input: written once,
        // and skipped in the others that have it too.
        let Some(min) = cursors.iter().filter_map(|c| c.head.as_ref().map(|e| e.key())).min() else { break };
        let mut written = false;
        for cur in cursors.iter_mut() {
            if cur.head.as_ref().map(|e| e.key()) == Some(min) {
                if !written {
                    if kind != Some(min.0) && !batch.is_empty() {
                        batches.push(std::mem::replace(&mut batch, Vec::with_capacity(BLOCK_SIZE)));
                    }
                    kind = Some(min.0);
                    batch.push(cur.head.take().unwrap());
                    if batch.len() >= BLOCK_SIZE {
                        batches.push(std::mem::replace(&mut batch, Vec::with_capacity(BLOCK_SIZE)));
                    }
                    written = true;
                }
                cur.advance()?;
            }
        }
        if batches.len() >= WINDOW {
            flush(&mut batches)?;
        }
    }
    if !batch.is_empty() {
        batches.push(batch);
    }
    flush(&mut batches)?;
    drop(tx);
    writer.join().map_err(|_| "the writer panicked".to_string())?
}
