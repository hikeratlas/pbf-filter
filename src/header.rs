//! The OSMHeader block: what a file needs readers to support, its bounding
//! box and its replication state.

use crate::proto::{fields, put_bytes, put_uint};

/// A field of our own in the HeaderBlock, for a JSON record of how the file
/// was made (`filter`'s profiles' rules, the source's size). Readers skip
/// fields they don't know, as protocol buffers require.
pub const RECORD_FIELD: u32 = 4242;

/// A HeaderBlock for our output: the input's bounding box, source and
/// replication fields (if any), its required features, as optional
/// features only `Sort.Type_then_ID` if `sorted` (not, e.g., `Has_Metadata`,
/// since metadata isn't written), and `record`, if any.
pub fn header_block(input: Option<&[u8]>, sorted: bool, record: Option<&serde_json::Value>) -> Vec<u8> {
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
    if let Some(r) = record {
        put_bytes(&mut out, RECORD_FIELD, r.to_string().as_bytes());
    }
    out
}

/// The record a header carries, if any.
pub fn record(header: &[u8]) -> Option<serde_json::Value> {
    fields(header).find(|(f, _)| *f == RECORD_FIELD).and_then(|(_, v)| serde_json::from_slice(v.bytes()).ok())
}

pub fn is_sorted(header: &[u8]) -> bool {
    fields(header).any(|(f, v)| f == 5 && v.bytes() == b"Sort.Type_then_ID")
}
