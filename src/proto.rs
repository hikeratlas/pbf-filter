//! The little of protocol buffers that PBF needs: reading fields, packed
//! varints and zigzag deltas, and writing them back.

#[derive(Debug, Clone, Copy)]
pub enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    // Not used by PBF, but they must be skipped over.
    Fixed64(#[allow(dead_code)] u64),
    Fixed32(#[allow(dead_code)] u32),
}

impl<'a> Value<'a> {
    pub fn varint(self) -> u64 {
        match self {
            Value::Varint(v) => v,
            _ => 0,
        }
    }
    pub fn bytes(self) -> &'a [u8] {
        match self {
            Value::Bytes(b) => b,
            _ => &[],
        }
    }
}

/// The fields of a message, in order.
pub struct Fields<'a> {
    buf: &'a [u8],
    pos: usize,
}

pub fn fields(buf: &[u8]) -> Fields<'_> {
    Fields { buf, pos: 0 }
}

#[inline]
fn read_varint(buf: &[u8], pos: &mut usize) -> u64 {
    let mut v = 0u64;
    let mut shift = 0;
    while *pos < buf.len() {
        let b = buf[*pos];
        *pos += 1;
        v |= ((b & 0x7f) as u64) << shift;
        if b < 0x80 {
            return v;
        }
        shift += 7;
    }
    v
}

impl<'a> Iterator for Fields<'a> {
    type Item = (u32, Value<'a>);
    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let key = read_varint(self.buf, &mut self.pos);
        let (field, wire) = ((key >> 3) as u32, key & 7);
        let v = match wire {
            0 => Value::Varint(read_varint(self.buf, &mut self.pos)),
            1 => {
                let b = self.buf.get(self.pos..self.pos + 8)?;
                self.pos += 8;
                Value::Fixed64(u64::from_le_bytes(b.try_into().unwrap()))
            }
            2 => {
                let n = read_varint(self.buf, &mut self.pos) as usize;
                let b = self.buf.get(self.pos..self.pos + n)?;
                self.pos += n;
                Value::Bytes(b)
            }
            5 => {
                let b = self.buf.get(self.pos..self.pos + 4)?;
                self.pos += 4;
                Value::Fixed32(u32::from_le_bytes(b.try_into().unwrap()))
            }
            _ => return None,
        };
        Some((field, v))
    }
}

/// A packed run of varints.
pub struct Varints<'a> {
    buf: &'a [u8],
    pos: usize,
}

pub fn varints(buf: &[u8]) -> Varints<'_> {
    Varints { buf, pos: 0 }
}

impl Iterator for Varints<'_> {
    type Item = u64;
    #[inline]
    fn next(&mut self) -> Option<u64> {
        if self.pos >= self.buf.len() {
            None
        } else {
            Some(read_varint(self.buf, &mut self.pos))
        }
    }
}

#[inline]
pub fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

#[inline]
pub fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

/// A packed run of zigzag-encoded deltas, summed back up.
pub fn deltas(buf: &[u8]) -> impl Iterator<Item = i64> + '_ {
    let mut acc = 0i64;
    varints(buf).map(move |v| {
        acc += unzigzag(v);
        acc
    })
}

// ---- writing ---------------------------------------------------------------

pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub fn put_key(out: &mut Vec<u8>, field: u32, wire: u8) {
    put_varint(out, ((field as u64) << 3) | wire as u64);
}

pub fn put_uint(out: &mut Vec<u8>, field: u32, v: u64) {
    put_key(out, field, 0);
    put_varint(out, v);
}

pub fn put_bytes(out: &mut Vec<u8>, field: u32, b: &[u8]) {
    put_key(out, field, 2);
    put_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

/// A packed field of varints. Empty runs are left out.
pub fn put_packed(out: &mut Vec<u8>, field: u32, vals: impl IntoIterator<Item = u64>, scratch: &mut Vec<u8>) {
    scratch.clear();
    for v in vals {
        put_varint(scratch, v);
    }
    if !scratch.is_empty() {
        put_bytes(out, field, scratch);
    }
}

/// A packed field of zigzag-encoded deltas.
pub fn put_deltas(out: &mut Vec<u8>, field: u32, vals: impl IntoIterator<Item = i64>, scratch: &mut Vec<u8>) {
    let mut prev = 0i64;
    put_packed(
        out,
        field,
        vals.into_iter().map(|v| {
            let d = v - prev;
            prev = v;
            zigzag(d)
        }),
        scratch,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zigzag_round_trips() {
        for v in [0i64, 1, -1, 63, -64, i64::MAX, i64::MIN, 123456789012] {
            assert_eq!(unzigzag(zigzag(v)), v);
        }
    }

    #[test]
    fn deltas_round_trip() {
        let vals = [5i64, 7, 3, 1_000_000_000_000, -2];
        let (mut out, mut scratch) = (vec![], vec![]);
        put_deltas(&mut out, 1, vals, &mut scratch);
        let (field, v) = fields(&out).next().unwrap();
        assert_eq!(field, 1);
        assert_eq!(deltas(v.bytes()).collect::<Vec<_>>(), vals);
    }
}
