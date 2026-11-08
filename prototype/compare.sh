#!/bin/bash
# Usage: prototype/compare.sh BASEMAP_DIR INPUT.osm.pbf WORK_DIR BBOX
#
# For every tilemaker profile in a hikeratlas/basemap checkout (slices/*.lua
# and the exports), filters INPUT down to what that profile needs
# (filter.py), runs the profile on INPUT and on the filtered PBF, and
# compares the outputs: features tile by tile for a slice, lines for an
# export. Same output means the filter kept everything the profile reads.
#
# TILEMAKER is the tilemaker binary (default: tilemaker). The checkout
# needs what the profiles read besides the PBF: ../qrank.db, the GeoJSON
# inputs. A slice's shapefile layers are left out of both runs.
set -uo pipefail
basemap=$(readlink -f "$1") pbf=$(readlink -f "$2") work=$(readlink -f "$3") bbox=$4
here=$(dirname "$(readlink -f "$0")")
tm=${TILEMAKER:-tilemaker}
mkdir -p "$work"
printf '%-24s %9s %9s  %s\n' profile full_MB filtered_MB result
for lua in "$basemap"/slices/*.lua; do
  name=$(basename "$lua" .lua)
  json="$basemap/slices/$name.json"
  [ -f "$json" ] || continue
  filtered="$work/slice-$name.osm.pbf"
  python3 "$here/filter.py" "$pbf" "$filtered" "$lua" 2> "$work/slice-$name.filter.log" || { echo "$name: filter failed"; continue; }
  # Without shapefile layers (e.g. the coastline): not from the PBF.
  python3 -c "
import json, sys
c = json.load(open(sys.argv[1]))
c['layers'] = {k: v for k, v in c['layers'].items() if not str(v.get('source', '')).endswith('.shp')}
json.dump(c, open(sys.argv[2], 'w'))" "$json" "$basemap/slices/compare-$name.json"
  for v in full filtered; do
    input=$pbf; [ $v = filtered ] && input=$filtered
    rm -f "$work/slice-$name-$v.pmtiles"
    (cd "$basemap/slices" && "$tm" --input "$input" --output "$work/slice-$name-$v.pmtiles" \
      --config "compare-$name.json" --process "$name.lua" --bbox "$bbox" > "$work/slice-$name-$v.log" 2>&1) \
      || echo "$name ($v): tilemaker failed, see $work/slice-$name-$v.log"
  done
  rm -f "$basemap/slices/compare-$name.json"
  result=$(python3 "$here/compare_pmtiles.py" "$work/slice-$name-full.pmtiles" "$work/slice-$name-filtered.pmtiles" 2>&1 | head -1)
  printf '%-24s %9.1f %9.1f  %s\n' "slices/$name" "$(du -m "$pbf" | cut -f1)" "$(echo "$(stat -c %s "$filtered") / 1048576" | bc -l)" "$result"
done
for lua in "$basemap"/export/*.lua; do
  name=$(basename "$lua" .lua)
  grep -q '^function \(way\|node\|relation\)_function' "$lua" || continue  # modules
  filtered="$work/export-$name.osm.pbf"
  python3 "$here/filter.py" "$pbf" "$filtered" "$lua" 2> "$work/export-$name.filter.log" || { echo "$name: filter failed"; continue; }
  for v in full filtered; do
    input=$pbf; [ $v = filtered ] && input=$filtered
    mkdir -p "$work/export-$v"
    (cd "$work/export-$v" && TILEMAKER="$tm" BBOX="$bbox" "$basemap/export/export" "$input" "$name" > "$work/export-$name-$v.log" 2>&1) \
      || echo "$name ($v): export failed, see $work/export-$name-$v.log"
  done
  a="$work/export-full/$name.geojsonl" b="$work/export-filtered/$name.geojsonl"
  if cmp -s <(sort "$a") <(sort "$b"); then result="$(wc -l < "$a") lines, identical"
  else result="DIFFERENT: $(wc -l < "$a") vs $(wc -l < "$b") lines, $(comm -3 <(sort "$a") <(sort "$b") | wc -l) differ"; fi
  printf '%-24s %9.1f %9.1f  %s\n' "export/$name" "$(du -m "$pbf" | cut -f1)" "$(echo "$(stat -c %s "$filtered") / 1048576" | bc -l)" "$result"
done
