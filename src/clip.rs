//! `clip`: the features of a GeoJSON-lines file (one feature per line, as
//! tilemaker and hikeratlas/basemap's exports write them) whose bounds reach
//! a box, in their order.
//!
//! A feature's bounds come from the numbers in its "coordinates" arrays (all
//! of them, for a GeometryCollection), read straight from the text: nothing
//! is parsed as JSON. A line whose coordinates can't be read that way is
//! kept, so a feature is never dropped that reaches the box. Lines are
//! checked in parallel, in chunks of the memory-mapped file.

use memchr::memmem;
use memmap2::Mmap;
use rayon::prelude::*;
use std::fs::File;
use std::io::{BufWriter, Write};

pub struct Stats {
    pub lines: usize,
    pub kept: usize,
    pub unread: usize,
}

const KEY: &[u8] = b"\"coordinates\"";
// Bounds may be off by a rounding error (see number): a feature this close
// to the box (about 0.1 mm) is kept.
const SLACK: f64 = 1e-9;
const CHUNK: usize = 16 << 20;

/// Min lon, min lat, max lon, max lat of a line's coordinates, or None if
/// it has none or they can't be read.
pub fn bounds(line: &[u8]) -> Option<[f64; 4]> {
    let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    let mut found = false;
    let mut from = 0;
    let finder = memmem::Finder::new(KEY);
    while let Some(k) = finder.find(&line[from..]) {
        let mut i = from + k + KEY.len();
        // `: [`, with any spaces.
        while i < line.len() && (line[i] == b' ' || line[i] == b':') {
            i += 1;
        }
        if i >= line.len() || line[i] != b'[' {
            return None;
        }
        // Walk the nested arrays. In the innermost one (a position), the
        // first number is the longitude, the second the latitude; a third
        // (elevation) is ignored.
        let mut depth = 0usize;
        let mut pos = 0usize;
        while i < line.len() {
            match line[i] {
                b'[' => {
                    depth += 1;
                    pos = 0;
                    i += 1;
                }
                b']' => {
                    depth -= 1;
                    i += 1;
                    if depth == 0 {
                        break;
                    }
                }
                b',' | b' ' | b'\t' => i += 1,
                b'-' | b'+' | b'.' | b'0'..=b'9' => {
                    let start = i;
                    while i < line.len() && matches!(line[i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                        i += 1;
                    }
                    let v = number(&line[start..i])?;
                    match pos {
                        0 => {
                            b[0] = b[0].min(v);
                            b[2] = b[2].max(v);
                        }
                        1 => {
                            b[1] = b[1].min(v);
                            b[3] = b[3].max(v);
                            found = true;
                        }
                        _ => {}
                    }
                    pos += 1;
                }
                _ => return None,
            }
        }
        if depth != 0 {
            return None;
        }
        from = i;
    }
    if found { Some(b) } else { None }
}

/// A JSON number: plain decimals (as the exports write coordinates) quickly,
/// to within a rounding error or two; anything else (an exponent, too many
/// digits) by Rust's exact parser.
fn number(s: &[u8]) -> Option<f64> {
    const POW10: [f64; 19] = [1.0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18];
    let (neg, digits) = match s.first() {
        Some(b'-') => (true, &s[1..]),
        _ => (false, s),
    };
    let (mut mant, mut n, mut frac, mut dot) = (0u64, 0usize, 0usize, false);
    for &c in digits {
        match c {
            b'0'..=b'9' if n < 18 => {
                mant = mant * 10 + (c - b'0') as u64;
                n += 1;
                frac += dot as usize;
            }
            b'.' if !dot => dot = true,
            _ => return std::str::from_utf8(s).ok()?.parse().ok(),
        }
    }
    if n == 0 {
        return None;
    }
    let v = mant as f64 / POW10[frac];
    Some(if neg { -v } else { v })
}

fn reaches(b: &[f64; 4], bx: &[f64; 4]) -> bool {
    !(b[2] < bx[0] - SLACK || b[0] > bx[2] + SLACK || b[3] < bx[1] - SLACK || b[1] > bx[3] + SLACK)
}

/// Writes the lines of INPUT whose features reach BOX to OUTPUT.
pub fn clip(input: &str, output: &str, bx: [f64; 4]) -> Result<Stats, String> {
    let f = File::open(input).map_err(|e| format!("{input}: {e}"))?;
    let len = f.metadata().map_err(|e| format!("{input}: {e}"))?.len();
    // An empty file can't be mapped; it clips to an empty file.
    let map = if len == 0 { None } else { Some(unsafe { Mmap::map(&f) }.map_err(|e| format!("{input}: {e}"))?) };
    let data: &[u8] = map.as_deref().unwrap_or(&[]);
    // Chunks ending at line ends.
    let mut cuts = vec![0];
    let mut at = 0;
    while at < data.len() {
        let mut end = (at + CHUNK).min(data.len());
        while end < data.len() && data[end - 1] != b'\n' {
            end += 1;
        }
        cuts.push(end);
        at = end;
    }
    let parts: Vec<(Vec<&[u8]>, usize, usize)> = cuts
        .par_windows(2)
        .map(|w| {
            let (mut kept, mut lines, mut unread) = (Vec::new(), 0, 0);
            for line in data[w[0]..w[1]].split_inclusive(|&c| c == b'\n') {
                if line.iter().all(|c| c.is_ascii_whitespace()) {
                    continue;
                }
                lines += 1;
                match bounds(line) {
                    Some(b) if !reaches(&b, &bx) => {}
                    Some(_) => kept.push(line),
                    None => {
                        unread += 1;
                        kept.push(line);
                    }
                }
            }
            (kept, lines, unread)
        })
        .collect();
    let mut out = BufWriter::with_capacity(1 << 20, File::create(output).map_err(|e| format!("{output}: {e}"))?);
    let mut s = Stats { lines: 0, kept: 0, unread: 0 };
    for (kept, lines, unread) in parts {
        s.lines += lines;
        s.unread += unread;
        s.kept += kept.len();
        for line in kept {
            out.write_all(line).map_err(|e| format!("{output}: {e}"))?;
            if !line.ends_with(b"\n") {
                out.write_all(b"\n").map_err(|e| format!("{output}: {e}"))?;
            }
        }
    }
    out.flush().map_err(|e| format!("{output}: {e}"))?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point() {
        let l = br#"{"type":"Feature","geometry":{"type":"Point","coordinates":[-80.304,43.43688]},"properties":{"kind":"viewpoint"}}"#;
        assert_eq!(bounds(l), Some([-80.304, 43.43688, -80.304, 43.43688]));
    }

    #[test]
    fn multipolygon_and_elevation() {
        let l = br#"{"geometry":{"coordinates":[[[[1,2,900],[3,-4,950]]],[[[-5,6]]]],"type":"MultiPolygon"}}"#;
        assert_eq!(bounds(l), Some([-5.0, -4.0, 3.0, 6.0]));
    }

    #[test]
    fn collection_and_spaces() {
        let l = br#"{"geometry": {"type": "GeometryCollection", "geometries": [{"type": "Point", "coordinates": [1.5, 2]}, {"type": "LineString", "coordinates": [[10, 20], [11, 1e1]]}]}}"#;
        assert_eq!(bounds(l), Some([1.5, 2.0, 11.0, 20.0]));
    }

    #[test]
    fn numbers() {
        for s in ["0", "-0.5", "123.456789", "-115.7523181", "1e1", "-2.5E-3", "12345678901234567890.5"] {
            let want: f64 = s.parse().unwrap();
            let got = number(s.as_bytes()).unwrap();
            assert!((got - want).abs() <= want.abs() * 1e-15, "{s}: {got} vs {want}");
        }
        assert_eq!(number(b"-"), None);
        assert_eq!(number(b"1.2.3"), None);
    }

    #[test]
    fn unreadable_is_none() {
        assert_eq!(bounds(br#"{"geometry":null}"#), None);
        assert_eq!(bounds(br#"{"geometry":{"coordinates":[1,"x"]}}"#), None);
        assert_eq!(bounds(br#"{"geometry":{"coordinates":[[1,2]"#), None);
    }

    #[test]
    fn clips_in_order_keeping_unreadable() {
        let dir = std::env::temp_dir().join(format!("clip-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (i, o) = (dir.join("in.geojsonl"), dir.join("out.geojsonl"));
        std::fs::write(
            &i,
            concat!(
                "{\"geometry\":{\"coordinates\":[0.5,0.5]},\"n\":1}\n",
                "{\"geometry\":{\"coordinates\":[50,50]},\"n\":2}\n",
                "\n",
                "{\"geometry\":null,\"n\":3}\n",
                "{\"geometry\":{\"coordinates\":[[-10,-10],[10,10]]},\"n\":4}",
            ),
        )
        .unwrap();
        let s = clip(i.to_str().unwrap(), o.to_str().unwrap(), [0.0, 0.0, 1.0, 1.0]).unwrap();
        let out = std::fs::read_to_string(&o).unwrap();
        assert_eq!((s.lines, s.kept, s.unread), (4, 3, 1));
        assert_eq!(
            out,
            "{\"geometry\":{\"coordinates\":[0.5,0.5]},\"n\":1}\n{\"geometry\":null,\"n\":3}\n{\"geometry\":{\"coordinates\":[[-10,-10],[10,10]]},\"n\":4}\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
