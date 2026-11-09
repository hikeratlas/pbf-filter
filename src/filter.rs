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
pub fn first_of(data: &[u8], blobs: &[BlobRef], mut lo: usize, kind: Kind) -> Result<usize, String> {
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

/// A sorted file's header block, and where its node, way and relation
/// blobs start: (header, first node blob, first way blob, first relation blob).
pub fn sections(input: &str, data: &[u8], blobs: &[BlobRef]) -> Result<(Vec<u8>, usize, usize, usize), String> {
    let header = blobs.iter().position(|b| b.is_header).ok_or("no OSMHeader block")?;
    let head = pbf::read_blob(data, &blobs[header])?;
    if !crate::header::is_sorted(&head) {
        return Err(format!("{input} isn't marked sorted (Sort.Type_then_ID)"));
    }
    let start = header + 1;
    let first_way = first_of(data, blobs, start, Kind::Ways)?;
    let first_rel = first_of(data, blobs, first_way, Kind::Relations)?;
    Ok((head, start, first_way, first_rel))
}

pub fn tag_bytes<'a>(b: &Block<'a>, tags: &[(u32, u32)]) -> Vec<(&'a [u8], &'a [u8])> {
    tags.iter().map(|&(k, v)| (b.string(k), b.string(v))).collect()
}

/// Filters each window's blocks in parallel with `f`, which turns a block
/// into kept entities, one builder per output, and writes each output's
/// blocks in order.
fn pass<W: Write, F>(data: &[u8], blobs: &[BlobRef], c: Compression, outs: &mut [W], f: F) -> Result<(), String>
where
    F: Fn(&Block, &mut [BlockBuilder], &mut [Vec<Vec<u8>>]) + Sync,
{
    let n = outs.len();
    for window in blobs.chunks(WINDOW) {
        // Each task takes a run of blocks, so its output blocks are full.
        let runs: Vec<&[BlobRef]> = window.chunks(window.len().div_ceil(rayon::current_num_threads()).max(1)).collect();
        let results: Vec<Result<Vec<Vec<Vec<u8>>>, String>> = runs
            .par_iter()
            .map(|run| {
                let mut builders: Vec<BlockBuilder> = (0..n).map(|_| BlockBuilder::new()).collect();
                let mut frames: Vec<Vec<Vec<u8>>> = vec![vec![]; n];
                let mut full: Vec<Vec<Vec<u8>>> = vec![vec![]; n];
                for blob in *run {
                    let raw = pbf::read_blob(data, blob)?;
                    let block = Block::parse(&raw);
                    f(&block, &mut builders, &mut full);
                    for (g, blocks) in full.iter_mut().enumerate() {
                        for block in blocks.drain(..) {
                            frames[g].push(frame("OSMData", &block, c));
                        }
                    }
                }
                for (g, b) in builders.iter_mut().enumerate() {
                    if b.len() > 0 {
                        frames[g].push(frame("OSMData", &b.finish(), c));
                    }
                }
                Ok(frames)
            })
            .collect();
        for r in results {
            for (out, frames) in outs.iter_mut().zip(r?) {
                for fr in frames {
                    out.write_all(&fr).map_err(|e| e.to_string())?;
                }
            }
        }
    }
    Ok(())
}

/// One filtered PBF to write: what `profiles` read.
pub struct Group {
    pub output: String,
    pub profiles: Vec<Profile>,
}

/// The highest node id in the file: the last node block's last.
fn max_node_id(data: &[u8], node_blobs: &[BlobRef]) -> Result<i64, String> {
    let Some(last) = node_blobs.last() else { return Ok(0) };
    let raw = pbf::read_blob(data, last)?;
    let mut max = 0;
    Block::parse(&raw).nodes(|n| max = max.max(n.id));
    Ok(max)
}

/// Memory the system can still give, from /proc/meminfo.
fn mem_available() -> Option<u64> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = info.lines().find(|l| l.starts_with("MemAvailable:"))?;
    Some(line.split_whitespace().nth(1)?.parse::<u64>().ok()? * 1024)
}

/// Writes one filtered PBF.
pub fn filter(input: &str, output: &str, profiles: Vec<Profile>, c: Compression) -> Result<Stats, String> {
    let mut stats = filter_groups(input, vec![Group { output: output.to_string(), profiles }], c, None)?;
    Ok(stats.remove(0))
}

/// Writes every group's filtered PBF, reading the input once for as many
/// groups as memory allows: each needs a bitmap of the node ids it keeps
/// (one bit per id up to the highest). `per_pass` overrides that.
pub fn filter_groups(input: &str, groups: Vec<Group>, c: Compression, per_pass: Option<usize>) -> Result<Vec<Stats>, String> {
    let file = File::open(input).map_err(|e| format!("{input}: {e}"))?;
    let map = unsafe { memmap2::Mmap::map(&file) }.map_err(|e| format!("{input}: {e}"))?;
    let data: &[u8] = &map;
    let blobs = pbf::index(data)?;
    let (head, start, first_way, first_rel) = sections(input, data, &blobs)?;
    let bitmap = (max_node_id(data, &blobs[start..first_way])? as u64 / 8).max(1);
    let per_pass = per_pass.unwrap_or_else(|| match mem_available() {
        // Leave a third for the blocks in flight and the rest of the system.
        Some(avail) => ((avail * 2 / 3) / bitmap).max(1) as usize,
        None => groups.len(),
    });
    // As few reads as that allows, with the groups spread evenly over them.
    let reads = groups.len().div_ceil(per_pass.max(1));
    let per_pass = groups.len().div_ceil(reads.max(1)).max(1);
    if per_pass < groups.len() {
        eprintln!("{} groups, {per_pass} per read of the input (node bitmaps of {} MB each)", groups.len(), bitmap / 1_000_000);
    }
    let mut stats = vec![];
    let mut groups = groups.into_iter().peekable();
    while groups.peek().is_some() {
        let batch: Vec<Group> = groups.by_ref().take(per_pass).collect();
        stats.extend(filter_pass(data, &blobs, &head, (start, first_way, first_rel), &batch, c)?);
    }
    Ok(stats)
}

/// Per-group state through the passes.
struct State {
    nodes: NodeSet,
    member_ways: HashSet<i64>,
    rel_frames: Vec<Vec<u8>>,
    stats: Stats,
}

fn filter_pass(
    data: &[u8],
    blobs: &[BlobRef],
    head: &[u8],
    (start, first_way, first_rel): (usize, usize, usize),
    groups: &[Group],
    c: Compression,
) -> Result<Vec<Stats>, String> {
    let t0 = Instant::now();
    let (node_blobs, way_blobs, rel_blobs) = (&blobs[start..first_way], &blobs[first_way..first_rel], &blobs[first_rel..]);
    let names: Vec<String> = groups
        .iter()
        .map(|g| std::path::Path::new(&g.output).file_name().map_or(g.output.clone(), |n| n.to_string_lossy().into_owned()))
        .collect();
    eprintln!(
        "{}: {} blocks: {} nodes, {} ways, {} relations",
        names.join(", "),
        blobs.len(),
        node_blobs.len(),
        way_blobs.len(),
        rel_blobs.len(),
    );
    let rules: Vec<Vec<Rules>> = groups.iter().map(|g| g.profiles.iter().map(|p| p.rules.clone()).collect()).collect();
    let mut state: Vec<State> = groups
        .iter()
        .map(|_| State {
            nodes: NodeSet::new(),
            member_ways: HashSet::new(),
            rel_frames: vec![],
            stats: Stats { nodes: (0, 0), ways: (0, 0), relations: (0, 0) },
        })
        .collect();

    // 1. Relations (Lua, so on this thread).
    let mut rel_out: Vec<BlockBuilder> = groups.iter().map(|_| BlockBuilder::new()).collect();
    for blob in rel_blobs {
        let raw = pbf::read_blob(data, blob)?;
        let block = Block::parse(&raw);
        let mut err = None;
        block.relations(|r| {
            if err.is_some() {
                return;
            }
            let tags = tag_bytes(&block, &r.tags);
            let multipolygon = tags.iter().any(|&(k, v)| k == b"type" && v == b"multipolygon");
            for (g, group) in groups.iter().enumerate() {
                let s = &mut state[g];
                s.stats.relations.0 += 1;
                let mut keep = false;
                for (p, rule) in group.profiles.iter().zip(&rules[g]) {
                    // tilemaker reads a relation (its relation_function, or a
                    // multipolygon or boundary's area) if relation_scan_function
                    // accepted it, or if its tags match way_keys, whatever its type.
                    keep |= rule.keeps_way(tags.iter().copied());
                    if !multipolygon {
                        match p.accepts(r.id, &tags) {
                            Ok(true) => keep = true,
                            Ok(false) => {}
                            Err(e) => err = Some(e),
                        }
                    }
                }
                if !keep {
                    continue;
                }
                s.stats.relations.1 += 1;
                for &(t, id, _) in &r.members {
                    match t {
                        Member::Way => {
                            s.member_ways.insert(id);
                        }
                        // tilemaker keeps the member nodes of a relation its
                        // relation_scan_function accepts (not a multipolygon's).
                        // Kept for every non-multipolygon kept, so the PBF still
                        // serves a profile that comes to accept one (see check).
                        Member::Node if !multipolygon => s.nodes.insert(id),
                        _ => {}
                    }
                }
                let out = &mut rel_out[g];
                out.add_relation(r.id, tags.iter().copied(), r.members.iter().map(|&(t, id, role)| (t, id, block.string(role))));
                if out.is_full() {
                    s.rel_frames.push(frame("OSMData", &out.finish(), c));
                }
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
    }
    for (s, out) in state.iter_mut().zip(&mut rel_out) {
        if out.len() > 0 {
            s.rel_frames.push(frame("OSMData", &out.finish(), c));
        }
    }
    eprintln!("relations ({:.1}s)", t0.elapsed().as_secs_f64());

    // 2. Ways, into a temporary file beside each output.
    let mut tmps = vec![];
    for g in groups {
        let path = format!("{}.ways.tmp", g.output);
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| format!("{path}: {e}"))?;
        tmps.push((path, BufWriter::new(f)));
    }
    let counts: Vec<(AtomicU64, AtomicU64)> = groups.iter().map(|_| (AtomicU64::new(0), AtomicU64::new(0))).collect();
    {
        let mut outs: Vec<&mut BufWriter<File>> = tmps.iter_mut().map(|(_, w)| w).collect();
        let state = &state;
        pass(data, way_blobs, c, &mut outs, |block, builders, full| {
            block.ways(|w| {
                let tags = tag_bytes(block, &w.tags);
                for (g, s) in state.iter().enumerate() {
                    counts[g].0.fetch_add(1, Relaxed);
                    if !(s.member_ways.contains(&w.id) || rules[g].iter().any(|r| r.keeps_way(tags.iter().copied()))) {
                        continue;
                    }
                    counts[g].1.fetch_add(1, Relaxed);
                    for &n in &w.refs {
                        s.nodes.insert(n);
                    }
                    let b = &mut builders[g];
                    b.add_way(w.id, tags.iter().copied(), &w.refs);
                    if b.is_full() {
                        full[g].push(b.finish());
                    }
                }
            });
        })?;
    }
    for (s, c) in state.iter_mut().zip(&counts) {
        s.stats.ways = (c.0.load(Relaxed), c.1.load(Relaxed));
    }
    eprintln!("ways ({:.1}s)", t0.elapsed().as_secs_f64());

    // 3. Nodes, then the ways and relations after them.
    let mut outs = vec![];
    for (g, group) in groups.iter().enumerate() {
        let mut out = BufWriter::new(File::create(&group.output).map_err(|e| format!("{}: {e}", group.output))?);
        let record = serde_json::json!({
            "source_bytes": data.len(),
            "profiles": rules[g].iter().map(|r| r.to_json()).collect::<Vec<_>>(),
        });
        out.write_all(&frame("OSMHeader", &crate::header::header_block(Some(head), true, Some(&record)), c))
            .map_err(|e| e.to_string())?;
        outs.push(out);
    }
    let counts: Vec<(AtomicU64, AtomicU64)> = groups.iter().map(|_| (AtomicU64::new(0), AtomicU64::new(0))).collect();
    {
        let state = &state;
        pass(data, node_blobs, c, &mut outs, |block, builders, full| {
            block.nodes(|n| {
                let tags = tag_bytes(block, &n.tags);
                for (g, s) in state.iter().enumerate() {
                    counts[g].0.fetch_add(1, Relaxed);
                    if !(s.nodes.contains(n.id) || rules[g].iter().any(|r| r.keeps_node(&tags))) {
                        continue;
                    }
                    counts[g].1.fetch_add(1, Relaxed);
                    let b = &mut builders[g];
                    b.add_node(n.id, n.lat, n.lon, tags.iter().copied());
                    if b.is_full() {
                        full[g].push(b.finish());
                    }
                }
            });
        })?;
    }
    for (s, c) in state.iter_mut().zip(&counts) {
        s.stats.nodes = (c.0.load(Relaxed), c.1.load(Relaxed));
    }
    eprintln!("nodes ({:.1}s)", t0.elapsed().as_secs_f64());
    let mut buf = vec![0u8; 1 << 20];
    for ((out, (path, tmp)), s) in outs.iter_mut().zip(tmps).zip(&state) {
        let mut tmp = tmp.into_inner().map_err(|e| e.to_string())?;
        tmp.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        loop {
            let n = tmp.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        }
        drop(tmp);
        let _ = std::fs::remove_file(&path);
        for fr in &s.rel_frames {
            out.write_all(fr).map_err(|e| e.to_string())?;
        }
        out.flush().map_err(|e| e.to_string())?;
    }
    Ok(state.into_iter().map(|s| s.stats).collect())
}
