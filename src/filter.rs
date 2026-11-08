//! Filtering a PBF to what a set of profiles reads, in one read of each block.
//!
//! The input must be sorted (nodes, then ways, then relations, each by id:
//! `Sort.Type_then_ID`), as Geofabrik's extracts are. The passes go:
//!
//! 1. relations: those whose tags match a profile's way_keys, or (if not
//!    multipolygons) that its relation_scan_function accepts, noting their
//!    member ways (and, for the latter, member nodes);
//! 2. ways: those matching way_keys or members of a kept relation, marking
//!    the nodes they use;
//! 3. nodes: those marked, or that a profile's node_function would see.
//!
//! The output is written in the same order (nodes, ways, relations): ways
//! and relations are held (ways in a temporary file) until the nodes are
//! written. Blocks are filtered in parallel, in windows that keep the order.

use crate::pbf::{self, Block, BlobRef, Kind, Member};
use crate::profile::{Profile, Rules};
use crate::write::{BlockBuilder, Compression, frame};
use rayon::prelude::*;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

/// Blocks per parallel window: big enough to keep every core busy, small
/// enough to bound memory.
const WINDOW: usize = 256;

/// A set of node ids, settable from many threads: a bitmap in 16M-id chunks,
/// allocated as ids land in them.
pub struct NodeSet {
    chunks: Vec<OnceLock<Box<[AtomicU64]>>>,
}

const CHUNK_BITS: u64 = 1 << 24;

impl NodeSet {
    pub fn new() -> NodeSet {
        // Up to 2^40 ids (tilemaker needs them under 2^34).
        NodeSet { chunks: (0..(1u64 << 40) / CHUNK_BITS).map(|_| OnceLock::new()).collect() }
    }
    fn chunk(&self, id: u64) -> &[AtomicU64] {
        self.chunks[(id / CHUNK_BITS) as usize].get_or_init(|| (0..CHUNK_BITS / 64).map(|_| AtomicU64::new(0)).collect())
    }
    pub fn insert(&self, id: i64) {
        let id = id as u64;
        let bit = id % CHUNK_BITS;
        self.chunk(id)[(bit / 64) as usize].fetch_or(1 << (bit % 64), Relaxed);
    }
    pub fn contains(&self, id: i64) -> bool {
        let id = id as u64;
        match self.chunks.get((id / CHUNK_BITS) as usize).and_then(|c| c.get()) {
            Some(c) => {
                let bit = id % CHUNK_BITS;
                c[(bit / 64) as usize].load(Relaxed) & (1 << (bit % 64)) != 0
            }
            None => false,
        }
    }
}

pub struct Stats {
    pub nodes: (u64, u64),
    pub ways: (u64, u64),
    pub relations: (u64, u64),
}

fn kind_of(data: &[u8], b: &BlobRef) -> Result<Kind, String> {
    let raw = pbf::read_blob(data, b)?;
    Ok(Block::parse(&raw).kind())
}

/// The first data blob (from `lo`) whose kind is at least `kind`, by
/// binary search: the file is sorted by kind.
fn first_of(data: &[u8], blobs: &[BlobRef], mut lo: usize, kind: Kind) -> Result<usize, String> {
    let mut hi = blobs.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if kind_of(data, &blobs[mid])? < kind {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

fn tag_bytes<'a>(b: &Block<'a>, tags: &[(u32, u32)]) -> Vec<(&'a [u8], &'a [u8])> {
    tags.iter().map(|&(k, v)| (b.string(k), b.string(v))).collect()
}

/// Filters each window's blocks in parallel with `f`, which turns a block
/// into kept entities in a builder, and writes the window's blocks in order.
fn pass<F>(data: &[u8], blobs: &[BlobRef], c: Compression, out: &mut impl Write, f: F) -> Result<u64, String>
where
    F: Fn(&Block, &mut BlockBuilder, &mut Vec<Vec<u8>>) + Sync,
{
    let mut written = 0u64;
    for window in blobs.chunks(WINDOW) {
        // Each task takes a run of blocks, so its output blocks are full.
        let runs: Vec<&[BlobRef]> = window.chunks(window.len().div_ceil(rayon::current_num_threads()).max(1)).collect();
        let outs: Vec<Result<(Vec<Vec<u8>>, u64), String>> = runs
            .par_iter()
            .map(|run| {
                let (mut b, mut frames, mut n) = (BlockBuilder::new(), vec![], 0u64);
                for blob in *run {
                    let raw = pbf::read_blob(data, blob)?;
                    let block = Block::parse(&raw);
                    let mut full = vec![];
                    f(&block, &mut b, &mut full);
                    for block in full {
                        frames.push(frame("OSMData", &block, c));
                    }
                }
                if b.len() > 0 {
                    n += b.len() as u64;
                    frames.push(frame("OSMData", &b.finish(), c));
                }
                Ok((frames, n))
            })
            .collect();
        for o in outs {
            let (frames, _) = o?;
            for fr in frames {
                out.write_all(&fr).map_err(|e| e.to_string())?;
                written += 1;
            }
        }
    }
    Ok(written)
}

pub fn filter(input: &str, output: &str, profiles: &[Profile], c: Compression) -> Result<Stats, String> {
    let t0 = Instant::now();
    let file = File::open(input).map_err(|e| format!("{input}: {e}"))?;
    let map = unsafe { memmap2::Mmap::map(&file) }.map_err(|e| format!("{input}: {e}"))?;
    let data: &[u8] = &map;
    let blobs = pbf::index(data)?;
    let header = blobs.iter().position(|b| b.is_header).ok_or("no OSMHeader block")?;
    let head = pbf::read_blob(data, &blobs[header])?;
    if !String::from_utf8_lossy(&head).contains("Sort.Type_then_ID") {
        return Err(format!("{input} isn't marked sorted (Sort.Type_then_ID)"));
    }
    let start = header + 1;
    let first_way = first_of(data, &blobs, start, Kind::Ways)?;
    let first_rel = first_of(data, &blobs, first_way, Kind::Relations)?;
    let (node_blobs, way_blobs, rel_blobs) = (&blobs[start..first_way], &blobs[first_way..first_rel], &blobs[first_rel..]);
    eprintln!(
        "{} blocks: {} nodes, {} ways, {} relations ({:.1}s)",
        blobs.len(),
        node_blobs.len(),
        way_blobs.len(),
        rel_blobs.len(),
        t0.elapsed().as_secs_f64()
    );
    let rules: Vec<Rules> = profiles.iter().map(|p| p.rules.clone()).collect();
    let mut stats = Stats { nodes: (0, 0), ways: (0, 0), relations: (0, 0) };

    // 1. Relations (Lua, so on this thread).
    let nodes = NodeSet::new();
    let mut member_ways: HashSet<i64> = HashSet::new();
    let mut rel_out = BlockBuilder::new();
    let mut rel_frames = vec![];
    for blob in rel_blobs {
        let raw = pbf::read_blob(data, blob)?;
        let block = Block::parse(&raw);
        let mut err = None;
        block.relations(|r| {
            stats.relations.0 += 1;
            if err.is_some() {
                return;
            }
            let tags = tag_bytes(&block, &r.tags);
            let multipolygon = tags.iter().any(|&(k, v)| k == b"type" && v == b"multipolygon");
            let (mut keep, mut scanned) = (false, false);
            for (p, rule) in profiles.iter().zip(&rules) {
                // tilemaker reads a relation (its relation_function, or a
                // multipolygon or boundary's area) if relation_scan_function
                // accepted it, or if its tags match way_keys, whatever its type.
                keep |= rule.keeps_way(tags.iter().copied());
                if !multipolygon {
                    match p.accepts(r.id, &tags) {
                        Ok(true) => {
                            keep = true;
                            scanned = true;
                        }
                        Ok(false) => {}
                        Err(e) => err = Some(e),
                    }
                }
            }
            if !keep {
                return;
            }
            stats.relations.1 += 1;
            for &(t, id, _) in &r.members {
                match t {
                    Member::Way => {
                        member_ways.insert(id);
                    }
                    // tilemaker keeps an accepted relation's member nodes,
                    // not a multipolygon's.
                    Member::Node if scanned => nodes.insert(id),
                    _ => {}
                }
            }
            rel_out.add_relation(r.id, tags.iter().copied(), r.members.iter().map(|&(t, id, role)| (t, id, block.string(role))));
            if rel_out.is_full() {
                rel_frames.push(frame("OSMData", &rel_out.finish(), c));
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
    }
    if rel_out.len() > 0 {
        rel_frames.push(frame("OSMData", &rel_out.finish(), c));
    }
    eprintln!("relations: kept {} of {} ({:.1}s)", stats.relations.1, stats.relations.0, t0.elapsed().as_secs_f64());

    // 2. Ways, into a temporary file beside the output.
    let tmp_path = format!("{output}.ways.tmp");
    let tmp_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp_path)
        .map_err(|e| format!("{tmp_path}: {e}"))?;
    let mut tmp = BufWriter::new(tmp_file);
    let way_counts = (AtomicU64::new(0), AtomicU64::new(0));
    pass(data, way_blobs, c, &mut tmp, |block, b, full| {
        block.ways(|w| {
            way_counts.0.fetch_add(1, Relaxed);
            let tags = tag_bytes(block, &w.tags);
            if !(member_ways.contains(&w.id) || rules.iter().any(|r| r.keeps_way(tags.iter().copied()))) {
                return;
            }
            way_counts.1.fetch_add(1, Relaxed);
            for &n in &w.refs {
                nodes.insert(n);
            }
            b.add_way(w.id, tags.iter().copied(), &w.refs);
            if b.is_full() {
                full.push(b.finish());
            }
        });
    })?;
    let mut tmp = tmp.into_inner().map_err(|e| e.to_string())?;
    stats.ways = (way_counts.0.into_inner(), way_counts.1.into_inner());
    eprintln!("ways: kept {} of {} ({:.1}s)", stats.ways.1, stats.ways.0, t0.elapsed().as_secs_f64());

    // 3. Nodes, then the ways and relations after them.
    let mut out = BufWriter::new(File::create(output).map_err(|e| format!("{output}: {e}"))?);
    let (fs, fe) = blobs[header].frame;
    out.write_all(&data[fs..fe]).map_err(|e| e.to_string())?;
    let node_counts = (AtomicU64::new(0), AtomicU64::new(0));
    pass(data, node_blobs, c, &mut out, |block, b, full| {
        block.nodes(|n| {
            node_counts.0.fetch_add(1, Relaxed);
            let tags = tag_bytes(block, &n.tags);
            if !(nodes.contains(n.id) || rules.iter().any(|r| r.keeps_node(&tags))) {
                return;
            }
            node_counts.1.fetch_add(1, Relaxed);
            b.add_node(n.id, n.lat, n.lon, tags.iter().copied());
            if b.is_full() {
                full.push(b.finish());
            }
        });
    })?;
    stats.nodes = (node_counts.0.into_inner(), node_counts.1.into_inner());
    eprintln!("nodes: kept {} of {} ({:.1}s)", stats.nodes.1, stats.nodes.0, t0.elapsed().as_secs_f64());
    tmp.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = tmp.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
    }
    drop(tmp);
    let _ = std::fs::remove_file(&tmp_path);
    for fr in &rel_frames {
        out.write_all(fr).map_err(|e| e.to_string())?;
    }
    out.flush().map_err(|e| e.to_string())?;
    Ok(stats)
}
