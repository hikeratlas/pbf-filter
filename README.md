# pbf-filter

Construct minimal pbfs for faster iteration.

Given a PBF and one or more tilemaker profiles (Lua), write a PBF with just
what those profiles read, so they produce the same output from it, faster.
What a profile reads is worked out from the profile itself: its `way_keys`
and `node_keys`, and its `relation_scan_function`, which is run on every
relation. No extra declarations.

## Usage

    pbf-filter filter INPUT.osm.pbf OUTPUT.osm.pbf PROFILE.lua [PROFILE.lua ...]
    pbf-filter relations INPUT.osm.pbf OUTPUT.relations.pbf
    pbf-filter check FILTERED.osm.pbf RELATIONS.pbf PROFILE.lua [PROFILE.lua ...]
    pbf-filter merge OUTPUT.osm.pbf INPUT.osm.pbf INPUT.osm.pbf [...]
    pbf-filter recompress INPUT.osm.pbf OUTPUT.osm.pbf

- `filter` reads each block of a sorted PBF once (relations, then ways,
  then nodes, in parallel) and writes zstd blocks (`--zlib` for zlib),
  which cldellow/tilemaker reads. It records the profiles' keys in the
  output's header.
- `check` says (exit 0, or 3 and why) whether a filtered PBF still serves
  profiles that may have changed: their keys must match no more than the
  recorded ones, and every relation they'd read (run on the `relations`
  sidecar, every relation's tags) must be in it. Logic changes, and scan
  functions accepting fewer relations, don't make it stale.
- `merge` combines sorted PBFs (e.g. neighbouring extracts), keeping one
  copy of what's in more than one; `recompress` rewrites one with zstd
  blocks. Every command drops metadata (versions, timestamps, users),
  which tilemaker doesn't read.

Releases (`v*` tags) publish Linux binaries for x86_64 and arm64.

## Prototype

`prototype/` checks the idea before the real tool (a single-pass Rust
program) is written:

- `filter.py INPUT.osm.pbf OUTPUT.osm.pbf PROFILE.lua...` writes the
  filtered PBF (pyosmium for the relation pass, the osmium tool for the
  filtering).
- `compare.sh BASEMAP_DIR INPUT.osm.pbf WORK_DIR BBOX` runs every profile in
  a hikeratlas/basemap checkout on the input and on its filtered PBF, and
  compares the outputs.

On Nova Scotia (89 MB), every basemap profile that reads the PBF gave
identical output from its filtered PBF, which ranged from under 0.1 MB
(campgrounds, peaks) to 35 MB (land).
