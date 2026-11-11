mod check;
mod clip;
mod copy;
mod filter;
mod header;
mod pbf;
mod profile;
mod proto;
mod write;

use std::path::{Path, PathBuf};
use std::time::Instant;
use write::Compression;

const USAGE: &str = "usage:
  pbf-filter filter INPUT.osm.pbf OUTPUT.osm.pbf PROFILE.lua [PROFILE.lua ...]
      A PBF with what the profiles (tilemaker Lua) read from INPUT, so
      running one on OUTPUT gives the same result as on INPUT. INPUT must
      be sorted (Sort.Type_then_ID).
  pbf-filter groups INPUT.osm.pbf GROUPS.json OUTDIR
      `filter` for each group in GROUPS ({\"name\": [\"PROFILE.lua\", ...]},
      paths relative to GROUPS' directory), as OUTDIR/<name>.zstd.pbf (or
      .osm.pbf with --zlib), in one read of INPUT: or as few as memory
      allows, each group needing a bitmap of node ids (one bit per id).
  pbf-filter merge OUTPUT.osm.pbf INPUT.osm.pbf INPUT.osm.pbf [...]
      Sorted PBFs (e.g. neighbouring extracts) as one sorted PBF, with one
      copy of what's in more than one. Filtered PBFs (made with the same
      profiles) or relations sidecars of neighbouring extracts merge into
      what filtering, or `relations` on, the merged extract would give.
  pbf-filter recompress INPUT.osm.pbf OUTPUT.osm.pbf
      The same data, without metadata (versions, timestamps, changesets,
      users), in zstd blocks.
  pbf-filter relations INPUT.osm.pbf OUTPUT.relations.pbf
      Every relation of INPUT, with its tags and no members: the sidecar
      `check` runs changed profiles on.
  pbf-filter check FILTERED.osm.pbf RELATIONS.pbf PROFILE.lua [PROFILE.lua ...]
      Whether FILTERED (from `filter`) still serves PROFILES, which may have
      changed since: exits 0 if so, 3 (saying why) if they'd read something
      it may not have, so a build can fall back to the whole PBF. RELATIONS
      is the sidecar from the same PBF.
  pbf-filter clip W,S,E,N INPUT.geojsonl OUTPUT.geojsonl [--margin DEG]
      The features of a GeoJSON-lines file (one per line) whose bounds
      reach the box, grown by DEG degrees on each side (default 0), in
      order. Bounds come from the coordinates' text; a line whose
      coordinates can't be read is kept.

options:
  --zlib       write zlib blocks (default: zstd, which cldellow/tilemaker reads)
  --level N    compression level (default: zstd 3, zlib 6)
  --per-pass N groups to write per read of INPUT (default: as memory allows)
  --node-index full|ranked|auto
               how groups mark kept nodes: a bitmap over every possible id
               (about 1.8 GB each), or, after a read of INPUT's node ids,
               one bit per node in it (default auto: full if every group
               fits at once)

Every command drops metadata, which tilemaker doesn't read.";

/// Loads profiles, saying what each declares.
fn load(paths: &[PathBuf]) -> Result<Vec<profile::Profile>, String> {
    let profiles: Vec<profile::Profile> = paths.iter().map(|p| profile::Profile::load(p)).collect::<Result<_, _>>()?;
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
    Ok(profiles)
}

/// The groups of a GROUPS.json, with their outputs in `outdir`.
fn read_groups(path: &Path, outdir: &Path, c: Compression) -> Result<Vec<filter::Group>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let obj = json.as_object().ok_or_else(|| format!("{}: not an object of groups", path.display()))?;
    let base = path.parent().unwrap_or(Path::new(""));
    let ext = match c {
        Compression::Zstd(_) => "zstd.pbf",
        Compression::Zlib(_) => "osm.pbf",
    };
    let mut groups = vec![];
    for (name, profiles) in obj {
        let list = profiles.as_array().filter(|a| !a.is_empty()).ok_or_else(|| format!("group {name}: not a list of profiles"))?;
        let paths: Vec<PathBuf> = list
            .iter()
            .map(|p| p.as_str().map(|s| base.join(s)).ok_or_else(|| format!("group {name}: {p} isn't a path")))
            .collect::<Result<_, _>>()?;
        eprintln!("group {name}:");
        groups.push(filter::Group { output: outdir.join(format!("{name}.{ext}")).display().to_string(), profiles: load(&paths)? });
    }
    Ok(groups)
}

fn size(p: &str) -> f64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) as f64 / 1e6
}

fn report(output: &str, s: &filter::Stats) {
    eprintln!(
        "{output}: {:.1} MB; nodes {} of {}, ways {} of {}, relations {} of {}",
        size(output),
        s.nodes.1,
        s.nodes.0,
        s.ways.1,
        s.ways.0,
        s.relations.1,
        s.relations.0,
    );
}

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
    let (mut zlib, mut level, mut per_pass, mut pos) = (false, None, None, vec![]);
    let mut node_index = filter::NodeIndex::Auto;
    let mut margin = 0.0f64;
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--zlib" => zlib = true,
            "--level" => level = Some(it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| die("--level N"))),
            "--node-index" => {
                node_index = match it.next().map(String::as_str) {
                    Some("full") => filter::NodeIndex::Full,
                    Some("ranked") => filter::NodeIndex::Ranked,
                    Some("auto") => filter::NodeIndex::Auto,
                    _ => die("--node-index full|ranked|auto"),
                }
            }
            "--per-pass" => {
                per_pass = Some(it.next().and_then(|v| v.parse().ok()).filter(|&n: &usize| n > 0).unwrap_or_else(|| die("--per-pass N")))
            }
            "--margin" => {
                margin = it.next().and_then(|v| v.parse().ok()).filter(|m: &f64| *m >= 0.0).unwrap_or_else(|| die("--margin DEG"))
            }
            // A negative longitude is a box, not an option.
            _ if a.starts_with("--") => die(&format!("unknown option {a}")),
            _ => pos.push(a.clone()),
        }
    }
    let c = if zlib { Compression::Zlib(level.unwrap_or(6) as u32) } else { Compression::Zstd(level.unwrap_or(3)) };
    let t0 = Instant::now();
    match cmd.as_str() {
        "filter" => {
            if pos.len() < 3 {
                die("filter needs INPUT, OUTPUT and at least one PROFILE");
            }
            let profiles = load(&pos[2..].iter().map(PathBuf::from).collect::<Vec<_>>()).unwrap_or_else(|e| fail(e));
            let s = filter::filter(&pos[0], &pos[1], profiles, c).unwrap_or_else(|e| fail(e));
            report(&pos[1], &s);
        }
        "groups" => {
            if pos.len() != 3 {
                die("groups needs INPUT, GROUPS and OUTDIR");
            }
            let groups = read_groups(Path::new(&pos[1]), Path::new(&pos[2]), c).unwrap_or_else(|e| fail(e));
            std::fs::create_dir_all(&pos[2]).unwrap_or_else(|e| fail(format!("{}: {e}", pos[2])));
            let outputs: Vec<String> = groups.iter().map(|g| g.output.clone()).collect();
            let stats = filter::filter_groups(&pos[0], groups, c, per_pass, node_index).unwrap_or_else(|e| fail(e));
            for (o, s) in outputs.iter().zip(&stats) {
                report(o, s);
            }
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
        "relations" => {
            if pos.len() != 2 {
                die("relations needs INPUT and OUTPUT");
            }
            let n = check::relations(&pos[0], &pos[1], c).unwrap_or_else(|e| fail(e));
            eprintln!("{}: {n} relations, {:.1} MB; {:.1}s", pos[1], size(&pos[1]), t0.elapsed().as_secs_f64());
        }
        "check" => {
            if pos.len() < 3 {
                die("check needs FILTERED, RELATIONS and at least one PROFILE");
            }
            let profiles: Vec<profile::Profile> =
                pos[2..].iter().map(|p| profile::Profile::load(Path::new(p))).collect::<Result<_, _>>().unwrap_or_else(|e| fail(e));
            match check::check(&pos[0], &pos[1], &profiles).unwrap_or_else(|e| fail(e)) {
                check::Verdict::Fresh { relations } => {
                    eprintln!("{}: still serves the profiles ({relations} relations checked; {:.1}s)", pos[0], t0.elapsed().as_secs_f64())
                }
                check::Verdict::Stale(why) => {
                    eprintln!("{}: stale: {why}", pos[0]);
                    std::process::exit(3)
                }
            }
        }
        "clip" => {
            if pos.len() != 3 {
                die("clip needs W,S,E,N, INPUT and OUTPUT");
            }
            let b: Vec<f64> = pos[0].split(',').map(|v| v.trim().parse().unwrap_or_else(|_| die("clip needs a box W,S,E,N"))).collect();
            if b.len() != 4 || b[0] > b[2] || b[1] > b[3] {
                die("clip needs a box W,S,E,N");
            }
            let bx = [b[0] - margin, b[1] - margin, b[2] + margin, b[3] + margin];
            let s = clip::clip(&pos[1], &pos[2], bx).unwrap_or_else(|e| fail(e));
            eprintln!(
                "{}: {} of {} features ({} unread, kept), {:.1} of {:.1} MB; {:.1}s",
                pos[2], s.kept, s.lines, s.unread, size(&pos[2]), size(&pos[1]), t0.elapsed().as_secs_f64()
            );
        }
        _ => die("unknown command"),
    }
}
