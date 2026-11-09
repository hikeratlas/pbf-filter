//! `recompress`: a PBF's data, re-encoded without metadata (versions,
//! timestamps, changesets, users: tilemaker reads none of it) and with zstd
//! (or zlib) blocks. Each block is re-encoded on its own, in parallel, so
//! the file keeps its blocks and order.
//!
//! `merge`: several sorted PBFs (e.g. Geofabrik's extracts of neighbouring
//! countries) as one sorted PBF, with one copy of what's in more than one
//! (the objects along their border, the same in each).

use crate::header;
use crate::pbf::{self, Block, BlobRef, Kind, Member};
use crate::write::{BlockBuilder, Compression, frame};
use rayon::prelude::*;
use std::fs::File;
use std::io::{BufWriter, Write};
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

/// An entity, owned, for merging across files.
enum Entity {
    Node { id: i64, lat: i64, lon: i64, tags: Vec<(Vec<u8>, Vec<u8>)> },
    Way { id: i64, tags: Vec<(Vec<u8>, Vec<u8>)>, refs: Vec<i64> },
    Relation { id: i64, tags: Vec<(Vec<u8>, Vec<u8>)>, members: Vec<(Member, i64, Vec<u8>)> },
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
        fn t(tags: &[(Vec<u8>, Vec<u8>)]) -> Vec<(&[u8], &[u8])> {
            tags.iter().map(|(k, v)| (k.as_slice(), v.as_slice())).collect()
        }
        match self {
            Entity::Node { id, lat, lon, tags } => b.add_node(*id, *lat, *lon, t(tags)),
            Entity::Way { id, tags, refs } => b.add_way(*id, t(tags), refs),
            Entity::Relation { id, tags, members } => {
                b.add_relation(*id, t(tags), members.iter().map(|(m, id, role)| (*m, *id, role.as_slice())))
            }
        }
    }
}

fn owned_tags(block: &Block, t: &[(u32, u32)]) -> Vec<(Vec<u8>, Vec<u8>)> {
    t.iter().map(|&(k, v)| (block.string(k).to_vec(), block.string(v).to_vec())).collect()
}

fn entities(block: &Block) -> Vec<Entity> {
    let tags = |t: &[(u32, u32)]| owned_tags(block, t);
    let mut out = vec![];
    block.nodes(|n| out.push(Entity::Node { id: n.id, lat: n.lat, lon: n.lon, tags: tags(&n.tags) }));
    block.ways(|w| out.push(Entity::Way { id: w.id, tags: tags(&w.tags), refs: w.refs }));
    block.relations(|r| {
        out.push(Entity::Relation {
            id: r.id,
            tags: tags(&r.tags),
            members: r.members.iter().map(|&(m, id, role)| (m, id, block.string(role).to_vec())).collect(),
        })
    });
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

pub fn merge(inputs: &[String], output: &str, c: Compression) -> Result<(), String> {
    let mut cursors = vec![];
    for path in inputs {
        let (_, rx) = reader(path.clone())?;
        let mut cur = Cursor { rx, buf: vec![].into_iter(), head: None };
        cur.advance()?;
        cursors.push(cur);
    }
    let mut out = BufWriter::new(File::create(output).map_err(|e| format!("{output}: {e}"))?);
    out.write_all(&frame("OSMHeader", &header::header_block(None, true, None), c)).map_err(|e| e.to_string())?;
    let (mut b, mut kind, mut raw_blocks) = (BlockBuilder::new(), None, vec![]);
    let flush = |raw: &mut Vec<Vec<u8>>, out: &mut BufWriter<File>| -> Result<(), String> {
        let frames: Vec<Vec<u8>> = raw.par_iter().map(|r| frame("OSMData", r, c)).collect();
        raw.clear();
        for f in frames {
            out.write_all(&f).map_err(|e| e.to_string())?;
        }
        Ok(())
    };
    loop {
        // The smallest (kind, id) at the head of any input: written once,
        // and skipped in the others that have it too.
        let Some(min) = cursors.iter().filter_map(|c| c.head.as_ref().map(|e| e.key())).min() else { break };
        let mut written = false;
        for cur in cursors.iter_mut() {
            if cur.head.as_ref().map(|e| e.key()) == Some(min) {
                if !written {
                    if kind != Some(min.0) && b.len() > 0 {
                        raw_blocks.push(b.finish());
                    }
                    kind = Some(min.0);
                    cur.head.as_ref().unwrap().add_to(&mut b);
                    if b.is_full() {
                        raw_blocks.push(b.finish());
                    }
                    written = true;
                }
                cur.advance()?;
            }
        }
        if raw_blocks.len() >= WINDOW {
            flush(&mut raw_blocks, &mut out)?;
        }
    }
    if b.len() > 0 {
        raw_blocks.push(b.finish());
    }
    flush(&mut raw_blocks, &mut out)?;
    out.flush().map_err(|e| e.to_string())
}
