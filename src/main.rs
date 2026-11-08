mod filter;
mod pbf;
mod profile;
mod proto;
mod write;

use std::path::Path;
use std::time::Instant;
use write::Compression;

const USAGE: &str = "usage: pbf-filter filter INPUT.osm.pbf OUTPUT.osm.pbf PROFILE.lua [PROFILE.lua ...]
  [--zlib]          write zlib blocks (default: zstd, which cldellow/tilemaker reads)
  [--level N]       compression level (default: zstd 3, zlib 6)

Writes a PBF with what the profiles (tilemaker Lua) read from INPUT, so
running one on OUTPUT gives the same result as on INPUT. INPUT must be
sorted (Sort.Type_then_ID).";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let die = |m: &str| -> ! {
        eprintln!("{m}\n\n{USAGE}");
        std::process::exit(2)
    };
    if args.first().map(String::as_str) != Some("filter") {
        die("unknown command");
    }
    let (mut zlib, mut level, mut pos) = (false, None, vec![]);
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--zlib" => zlib = true,
            "--level" => level = Some(it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| die("--level N"))),
            _ if a.starts_with("--") => die(&format!("unknown option {a}")),
            _ => pos.push(a.clone()),
        }
    }
    if pos.len() < 3 {
        die("need INPUT, OUTPUT and at least one PROFILE");
    }
    let c = if zlib { Compression::Zlib(level.unwrap_or(6) as u32) } else { Compression::Zstd(level.unwrap_or(3)) };
    let t0 = Instant::now();
    let profiles: Vec<profile::Profile> = pos[2..]
        .iter()
        .map(|p| profile::Profile::load(Path::new(p)))
        .collect::<Result<_, _>>()
        .unwrap_or_else(|e| {
            eprintln!("error: {e}");
            std::process::exit(1)
        });
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
    match filter::filter(&pos[0], &pos[1], &profiles, c) {
        Ok(s) => {
            let size = |p: &str| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) as f64 / 1e6;
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
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1)
        }
    }
}
