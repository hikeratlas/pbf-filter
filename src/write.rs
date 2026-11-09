//! Writing PBF blocks: nodes (as DenseNodes), ways and relations, each block
//! with its own string table, compressed with zstd or zlib.

use crate::pbf::Member;
use crate::proto::{put_bytes, put_deltas, put_packed, put_uint};
use std::collections::HashMap;
use std::io::Write;

/// Entities per block, as is usual for PBF writers.
pub const BLOCK_SIZE: usize = 8000;

#[derive(Clone, Copy)]
pub enum Compression {
    Zstd(i32),
    Zlib(u32),
}

#[derive(Default)]
pub struct BlockBuilder {
    strings: Vec<Vec<u8>>,
    index: HashMap<Vec<u8>, u32>,
    count: usize,
    // Dense nodes
    ids: Vec<i64>,
    lats: Vec<i64>,
    lons: Vec<i64>,
    keys_vals: Vec<u64>,
    any_node_tags: bool,
    // Ways and relations, as encoded messages
    ways: Vec<u8>,
    relations: Vec<u8>,
    scratch: Vec<u8>,
    msg: Vec<u8>,
}

impl BlockBuilder {
    pub fn new() -> BlockBuilder {
        let mut b = BlockBuilder::default();
        b.strings.push(vec![]); // string 0 is the empty string
        b
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_full(&self) -> bool {
        self.count >= BLOCK_SIZE
    }

    fn string(&mut self, s: &[u8]) -> u32 {
        if let Some(&i) = self.index.get(s) {
            return i;
        }
        let i = self.strings.len() as u32;
        self.strings.push(s.to_vec());
        self.index.insert(s.to_vec(), i);
        i
    }

    /// lat and lon in nanodegrees; written at the default granularity
    /// (100 nanodegrees).
    pub fn add_node<'s>(&mut self, id: i64, lat: i64, lon: i64, tags: impl IntoIterator<Item = (&'s [u8], &'s [u8])>) {
        self.ids.push(id);
        self.lats.push(lat.div_euclid(100));
        self.lons.push(lon.div_euclid(100));
        for (k, v) in tags {
            let (k, v) = (self.string(k), self.string(v));
            self.keys_vals.push(k as u64);
            self.keys_vals.push(v as u64);
            self.any_node_tags = true;
        }
        self.keys_vals.push(0);
        self.count += 1;
    }

    fn tags<'s>(&mut self, tags: impl IntoIterator<Item = (&'s [u8], &'s [u8])>) -> (Vec<u64>, Vec<u64>) {
        let (mut ks, mut vs) = (vec![], vec![]);
        for (k, v) in tags {
            ks.push(self.string(k) as u64);
            vs.push(self.string(v) as u64);
        }
        (ks, vs)
    }

    pub fn add_way<'s>(&mut self, id: i64, tags: impl IntoIterator<Item = (&'s [u8], &'s [u8])>, refs: &[i64]) {
        let (ks, vs) = self.tags(tags);
        let (mut msg, mut scratch) = (std::mem::take(&mut self.msg), std::mem::take(&mut self.scratch));
        msg.clear();
        put_uint(&mut msg, 1, id as u64);
        put_packed(&mut msg, 2, ks, &mut scratch);
        put_packed(&mut msg, 3, vs, &mut scratch);
        put_deltas(&mut msg, 8, refs.iter().copied(), &mut scratch);
        put_bytes(&mut self.ways, 3, &msg);
        (self.msg, self.scratch) = (msg, scratch);
        self.count += 1;
    }

    pub fn add_relation<'s>(
        &mut self,
        id: i64,
        tags: impl IntoIterator<Item = (&'s [u8], &'s [u8])>,
        members: impl IntoIterator<Item = (Member, i64, &'s [u8])>,
    ) {
        let (ks, vs) = self.tags(tags);
        let (mut roles, mut ids, mut types) = (vec![], vec![], vec![]);
        for (t, mid, role) in members {
            roles.push(self.string(role) as u64);
            ids.push(mid);
            types.push(match t {
                Member::Node => 0,
                Member::Way => 1,
                Member::Relation => 2,
            });
        }
        let (mut msg, mut scratch) = (std::mem::take(&mut self.msg), std::mem::take(&mut self.scratch));
        msg.clear();
        put_uint(&mut msg, 1, id as u64);
        put_packed(&mut msg, 2, ks, &mut scratch);
        put_packed(&mut msg, 3, vs, &mut scratch);
        put_packed(&mut msg, 8, roles, &mut scratch);
        put_deltas(&mut msg, 9, ids, &mut scratch);
        put_packed(&mut msg, 10, types, &mut scratch);
        put_bytes(&mut self.relations, 4, &msg);
        (self.msg, self.scratch) = (msg, scratch);
        self.count += 1;
    }

    /// The PrimitiveBlock, encoded; and the builder is emptied.
    pub fn finish(&mut self) -> Vec<u8> {
        let mut scratch = vec![];
        let mut st = vec![];
        for s in &self.strings {
            put_bytes(&mut st, 1, s);
        }
        let mut group = vec![];
        if !self.ids.is_empty() {
            let mut dense = vec![];
            put_deltas(&mut dense, 1, self.ids.iter().copied(), &mut scratch);
            put_deltas(&mut dense, 8, self.lats.iter().copied(), &mut scratch);
            put_deltas(&mut dense, 9, self.lons.iter().copied(), &mut scratch);
            if self.any_node_tags {
                put_packed(&mut dense, 10, self.keys_vals.iter().copied(), &mut scratch);
            }
            put_bytes(&mut group, 2, &dense);
        }
        group.extend_from_slice(&self.ways);
        group.extend_from_slice(&self.relations);
        let mut block = vec![];
        put_bytes(&mut block, 1, &st);
        put_bytes(&mut block, 2, &group);
        *self = BlockBuilder::new();
        block
    }
}

/// A blob frame: length, BlobHeader, Blob.
pub fn frame(kind: &str, raw: &[u8], c: Compression) -> Vec<u8> {
    let mut blob = vec![];
    put_uint(&mut blob, 2, raw.len() as u64);
    match c {
        Compression::Zstd(level) => put_bytes(&mut blob, 7, &zstd::bulk::compress(raw, level).expect("zstd")),
        Compression::Zlib(level) => {
            let mut e = flate2::write::ZlibEncoder::new(vec![], flate2::Compression::new(level));
            e.write_all(raw).unwrap();
            put_bytes(&mut blob, 3, &e.finish().unwrap());
        }
    }
    let mut header = vec![];
    put_bytes(&mut header, 1, kind.as_bytes());
    put_uint(&mut header, 3, blob.len() as u64);
    let mut out = Vec::with_capacity(4 + header.len() + blob.len());
    out.extend_from_slice(&(header.len() as u32).to_be_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(&blob);
    out
}
