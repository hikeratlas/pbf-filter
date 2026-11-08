# pbf-filter

Construct minimal pbfs for faster iteration.

Given a PBF and one or more tilemaker profiles (Lua), write a PBF with just
what those profiles read, so they produce the same output from it, faster.
What a profile reads is worked out from the profile itself: its `way_keys`
and `node_keys`, and its `relation_scan_function`, which is run on every
relation. No extra declarations.

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
