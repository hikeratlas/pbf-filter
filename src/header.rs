//! The OSMHeader block: what a file needs readers to support, its bounding
//! box and its replication state.

use crate::proto::{fields, put_bytes, put_uint};

/// A HeaderBlock for our output: the input's bounding box, source and
/// replication fields (if any), its required features, and as optional
/// features only `Sort.Type_then_ID` if `sorted` (not, e.g., `Has_Metadata`,
/// since metadata isn't written).
pub fn header_block(input: Option<&[u8]>, sorted: bool) -> Vec<u8> {
    let mut out = vec![];
    let mut required = vec![];
    if let Some(h) = input {
        for (f, v) in fields(h) {
            match f {
                // bbox, source, replication timestamp, sequence number, base url
                1 | 17 | 34 => put_bytes(&mut out, f, v.bytes()),
                32 | 33 => put_uint(&mut out, f, v.varint()),
                4 => required.push(v.bytes().to_vec()),
                _ => {}
            }
        }
    }
    for need in [&b"OsmSchema-V0.6"[..], b"DenseNodes"] {
        if !required.iter().any(|r| r == need) {
            required.push(need.to_vec());
        }
    }
    for r in &required {
        put_bytes(&mut out, 4, r);
    }
    if sorted {
        put_bytes(&mut out, 5, b"Sort.Type_then_ID");
    }
    put_bytes(&mut out, 16, concat!("pbf-filter ", env!("CARGO_PKG_VERSION")).as_bytes());
    out
}

pub fn is_sorted(header: &[u8]) -> bool {
    fields(header).any(|(f, v)| f == 5 && v.bytes() == b"Sort.Type_then_ID")
}
