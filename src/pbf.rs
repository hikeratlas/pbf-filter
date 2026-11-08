//! Reading PBF files: the file's blocks, and the nodes, ways and relations in
//! them. https://wiki.openstreetmap.org/wiki/PBF_Format
//!
//! A file is a sequence of (4-byte big-endian length, BlobHeader, Blob). A
//! Blob holds a HeaderBlock or a PrimitiveBlock, raw or compressed.

use crate::proto::{deltas, fields, unzigzag, varints};
use std::io::Read;

pub type Result<T> = std::result::Result<T, String>;

/// Where a blob's bytes are in the file.
#[derive(Clone, Copy, Debug)]
pub struct BlobRef {
    /// The whole frame (length, BlobHeader, Blob), for copying it as is.
    pub frame: (usize, usize),
    /// The Blob message.
    pub blob: (usize, usize),
    pub is_header: bool,
}

/// Every blob in the file, in order. Reads only the BlobHeaders.
pub fn index(data: &[u8]) -> Result<Vec<BlobRef>> {
    let mut out = vec![];
    let mut pos = 0;
    while pos < data.len() {
        let len = u32::from_be_bytes(data.get(pos..pos + 4).ok_or("truncated blob length")?.try_into().unwrap()) as usize;
        let header = data.get(pos + 4..pos + 4 + len).ok_or("truncated BlobHeader")?;
        let (mut kind, mut size) = (&[][..], 0usize);
        for (f, v) in fields(header) {
            match f {
                1 => kind = v.bytes(),
                3 => size = v.varint() as usize,
                _ => {}
            }
        }
        let start = pos + 4 + len;
        if start + size > data.len() {
            return Err(format!("truncated Blob at byte {pos}"));
        }
        out.push(BlobRef { frame: (pos, start + size), blob: (start, start + size), is_header: kind == b"OSMHeader" });
        pos = start + size;
    }
    Ok(out)
}

/// A blob's contents, decompressed.
pub fn read_blob(data: &[u8], b: &BlobRef) -> Result<Vec<u8>> {
    let blob = &data[b.blob.0..b.blob.1];
    let mut raw_size = 0usize;
    for (f, v) in fields(blob) {
        match f {
            2 => raw_size = v.varint() as usize,
            _ => {}
        }
    }
    for (f, v) in fields(blob) {
        match f {
            1 => return Ok(v.bytes().to_vec()),
            3 => {
                let mut out = Vec::with_capacity(raw_size);
                flate2::read::ZlibDecoder::new(v.bytes()).read_to_end(&mut out).map_err(|e| format!("zlib: {e}"))?;
                return Ok(out);
            }
            7 => {
                return zstd::bulk::decompress(v.bytes(), raw_size.max(1)).map_err(|e| format!("zstd: {e}"));
            }
            4 | 5 | 6 => return Err(format!("unsupported blob compression (field {f})")),
            _ => {}
        }
    }
    Err("blob with no data".into())
}

/// What a PrimitiveBlock holds. A sorted file's blocks hold one kind each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Nodes,
    Ways,
    Relations,
    Empty,
}

pub struct Block<'a> {
    pub strings: Vec<&'a [u8]>,
    granularity: i64,
    lat_offset: i64,
    lon_offset: i64,
    groups: Vec<(u32, &'a [u8])>,
}

pub type Tags = Vec<(u32, u32)>;

pub struct Node {
    pub id: i64,
    /// In nanodegrees.
    pub lat: i64,
    pub lon: i64,
    pub tags: Tags,
}

pub struct Way {
    pub id: i64,
    pub tags: Tags,
    pub refs: Vec<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Member {
    Node,
    Way,
    Relation,
}

pub struct Relation {
    pub id: i64,
    pub tags: Tags,
    /// (type, id, role string index)
    pub members: Vec<(Member, i64, u32)>,
}

impl<'a> Block<'a> {
    pub fn parse(buf: &'a [u8]) -> Block<'a> {
        let mut b = Block { strings: vec![], granularity: 100, lat_offset: 0, lon_offset: 0, groups: vec![] };
        for (f, v) in fields(buf) {
            match f {
                1 => b.strings = fields(v.bytes()).filter(|(f, _)| *f == 1).map(|(_, v)| v.bytes()).collect(),
                2 => {
                    for (gf, gv) in fields(v.bytes()) {
                        b.groups.push((gf, gv.bytes()));
                    }
                }
                17 => b.granularity = v.varint() as i64,
                19 => b.lat_offset = v.varint() as i64,
                20 => b.lon_offset = v.varint() as i64,
                _ => {}
            }
        }
        b
    }

    pub fn kind(&self) -> Kind {
        match self.groups.first().map(|g| g.0) {
            Some(1 | 2) => Kind::Nodes,
            Some(3) => Kind::Ways,
            Some(4) => Kind::Relations,
            _ => Kind::Empty,
        }
    }

    pub fn string(&self, i: u32) -> &'a [u8] {
        self.strings.get(i as usize).copied().unwrap_or(b"")
    }

    pub fn nodes(&self, mut f: impl FnMut(Node)) {
        for &(gf, msg) in &self.groups {
            match gf {
                2 => self.dense(msg, &mut f),
                1 => {
                    let (mut id, mut lat, mut lon, mut keys, mut vals) = (0, 0, 0, &[][..], &[][..]);
                    for (nf, v) in fields(msg) {
                        match nf {
                            1 => id = unzigzag(v.varint()),
                            2 => keys = v.bytes(),
                            3 => vals = v.bytes(),
                            8 => lat = unzigzag(v.varint()),
                            9 => lon = unzigzag(v.varint()),
                            _ => {}
                        }
                    }
                    let tags = varints(keys).zip(varints(vals)).map(|(k, v)| (k as u32, v as u32)).collect();
                    f(Node { id, lat: self.lat_offset + self.granularity * lat, lon: self.lon_offset + self.granularity * lon, tags });
                }
                _ => {}
            }
        }
    }

    fn dense(&self, msg: &[u8], f: &mut impl FnMut(Node)) {
        let (mut ids, mut lats, mut lons, mut kv) = (&[][..], &[][..], &[][..], &[][..]);
        for (nf, v) in fields(msg) {
            match nf {
                1 => ids = v.bytes(),
                8 => lats = v.bytes(),
                9 => lons = v.bytes(),
                10 => kv = v.bytes(),
                _ => {}
            }
        }
        let mut kv = varints(kv);
        for ((id, lat), lon) in deltas(ids).zip(deltas(lats)).zip(deltas(lons)) {
            let mut tags = vec![];
            // keys_vals: k, v, k, v, ..., 0 per node (absent if no node has tags)
            while let Some(k) = kv.next() {
                if k == 0 {
                    break;
                }
                let v = kv.next().unwrap_or(0);
                tags.push((k as u32, v as u32));
            }
            f(Node { id, lat: self.lat_offset + self.granularity * lat, lon: self.lon_offset + self.granularity * lon, tags });
        }
    }

    pub fn ways(&self, mut f: impl FnMut(Way)) {
        for &(gf, msg) in &self.groups {
            if gf != 3 {
                continue;
            }
            let (mut id, mut keys, mut vals, mut refs) = (0, &[][..], &[][..], &[][..]);
            for (wf, v) in fields(msg) {
                match wf {
                    1 => id = v.varint() as i64,
                    2 => keys = v.bytes(),
                    3 => vals = v.bytes(),
                    8 => refs = v.bytes(),
                    _ => {}
                }
            }
            let tags = varints(keys).zip(varints(vals)).map(|(k, v)| (k as u32, v as u32)).collect();
            f(Way { id, tags, refs: deltas(refs).collect() });
        }
    }

    pub fn relations(&self, mut f: impl FnMut(Relation)) {
        for &(gf, msg) in &self.groups {
            if gf != 4 {
                continue;
            }
            let (mut id, mut keys, mut vals, mut roles, mut memids, mut types) =
                (0, &[][..], &[][..], &[][..], &[][..], &[][..]);
            for (rf, v) in fields(msg) {
                match rf {
                    1 => id = v.varint() as i64,
                    2 => keys = v.bytes(),
                    3 => vals = v.bytes(),
                    8 => roles = v.bytes(),
                    9 => memids = v.bytes(),
                    10 => types = v.bytes(),
                    _ => {}
                }
            }
            let tags = varints(keys).zip(varints(vals)).map(|(k, v)| (k as u32, v as u32)).collect();
            let members = varints(types)
                .zip(deltas(memids))
                .zip(varints(roles))
                .map(|((t, id), r)| {
                    let t = match t {
                        0 => Member::Node,
                        1 => Member::Way,
                        _ => Member::Relation,
                    };
                    (t, id, r as u32)
                })
                .collect();
            f(Relation { id, tags, members });
        }
    }
}
