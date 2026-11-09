mod copy;
mod filter;
mod header;
mod pbf;
mod profile;
mod proto;
mod write;

use std::path::Path;
use std::time::Instant;
use write::Compression;

const USAGE: &str = "usage:
  pbf-filter filter INPUT.osm.pbf OUTPUT.osm.pbf PROFILE.lua [PROFILE.lua ...]
      A PBF with what the profiles (tilemaker Lua) read from INPUT, so
      running one on OUTPUT gives the same result as on INPUT. INPUT must
      be sorted (Sort.Type_then_ID).
  pbf-filter merge OUTPUT.osm.pbf INPUT.osm.pbf INPUT.osm.pbf [...]
      Sorted PBFs (e.g. neighbouring extracts) as one sorted PBF, with one
      copy of what's in more than one.
  pbf-filter recompress INPUT.osm.pbf OUTPUT.osm.pbf
      The same data, without metadata (versions, timestamps, changesets,
      users), in zstd blocks.

options:
  --zlib       write zlib blocks (default: zstd, which cldellow/tilemaker reads)
  --level N    compression level (default: zstd 3, zlib 6)

Every command drops metadata, which tilemaker doesn't read.";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let die = |m: &str| -> ! {
        eprintln!("{m}\n\n{USAGE}");
        std::process::exit(2)
    };
    let fail = |e: String| -> ! {
        eprintln!("error: {e}");
        std::process::exit(1)
    };
    let cmd = args.first().cloned().unwrap_or_default();
    let (mut zlib, mut level, mut pos) = (false, None, vec![]);
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--zlib" => zlib = true,
            "--level" => level = Some(it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| die("--level N"))),
            _ if a.starts_with("--") => die(&format!("unknown option {a}")),
            _ => pos.push(a.clone()),
        }
    }
    let c = if zlib { Compression::Zlib(level.unwrap_or(6) as u32) } else { Compression::Zstd(level.unwrap_or(3)) };
    let t0 = Instant::now();
    let size = |p: &str| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) as f64 / 1e6;
    match cmd.as_str() {
        "filter" => {
            if pos.len() < 3 {
                die("filter needs INPUT, OUTPUT and at least one PROFILE");
            }
            let profiles: Vec<profile::Profile> =
                pos[2..].iter().map(|p| profile::Profile::load(Path::new(p))).collect::<Result<_, _>>().unwrap_or_else(|e| fail(e));
            for p in &profiles {
                let r = &p.rules;
                eprintln!(
                    "{}: way_keys {}, node_keys {}, node_function {}, relation_scan_function {}",
                    r.name,
                    if r.way_keys.is_some() { "yes" } else { "none (every way)" },
                    if r.node_keys.is_some() { "yes" } else { "none" },
                    if r.node_function { "yes" } else { "no (or empty)" },
                    if p.has_scan() { "yes" } else { "no" }
                );
            }
            let s = filter::filter(&pos[0], &pos[1], &profiles, c).unwrap_or_else(|e| fail(e));
            eprintln!(
                "{}: {:.1} of {:.1} MB; nodes {} of {}, ways {} of {}, relations {} of {}; {:.1}s",
                pos[1],
                size(&pos[1]),
                size(&pos[0]),
                s.nodes.1,
                s.nodes.0,
                s.ways.1,
                s.ways.0,
                s.relations.1,
                s.relations.0,
                t0.elapsed().as_secs_f64()
            );
        }
        "merge" => {
            if pos.len() < 3 {
                die("merge needs OUTPUT and at least two INPUTs");
            }
            copy::merge(&pos[1..], &pos[0], c).unwrap_or_else(|e| fail(e));
            eprintln!("{}: {:.1} MB; {:.1}s", pos[0], size(&pos[0]), t0.elapsed().as_secs_f64());
        }
        "recompress" => {
            if pos.len() != 2 {
                die("recompress needs INPUT and OUTPUT");
            }
            copy::recompress(&pos[0], &pos[1], c).unwrap_or_else(|e| fail(e));
            eprintln!("{}: {:.1} of {:.1} MB; {:.1}s", pos[1], size(&pos[1]), size(&pos[0]), t0.elapsed().as_secs_f64());
        }
        _ => die("unknown command"),
    }
}
