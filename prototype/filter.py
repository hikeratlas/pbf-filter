#!/usr/bin/env python3
"""Prototype: the smallest PBF a tilemaker profile needs.

Usage: prototype/filter.py INPUT.osm.pbf OUTPUT.osm.pbf PROFILE.lua [PROFILE.lua ...]

Writes one PBF with everything the given profiles (Lua, as tilemaker runs
them) can see, so running a profile on it gives the same output as on the
whole input. Several profiles make one shared file: the union.

What a profile sees, following tilemaker's read of a PBF:
- ways matching its `way_keys` (all ways if it has none), and the nodes they
  use;
- nodes matching its `node_keys`; with no `node_keys`, every tagged node,
  unless its `node_function` is empty;
- multipolygon relations matching its `way_keys`;
- other relations its `relation_scan_function` accepts, which this runs on
  every relation, and their members (and the members' nodes).

This prototype is for checking the idea (are the outputs the same, how big
are the files?), not for speed: it uses pyosmium for the relation pass and
the osmium tool for the filtering. The real tool is to be a single pass.
"""
import os
import re
import subprocess
import sys
import tempfile

import lupa.luajit21 as lupa
import osmium


class Profile:
    def __init__(self, path):
        self.path = path
        self.name = os.path.basename(path)
        lua = lupa.LuaRuntime(unpack_returned_tuples=True)
        here = os.path.dirname(os.path.abspath(path))
        lua.execute(f'package.path = "{here}/?.lua;" .. package.path')
        # Modules that open databases or files when used: only the profile's
        # top-level declarations and relation_scan_function matter here.
        for mod in ('qrank', 'keydb', 'file_append'):
            lua.execute(f'package.preload["{mod}"] = function() return setmetatable({{}}, '
                        f'{{__index = function() return function() end end}}) end')
        # What relation_scan_function may call (tilemaker's API there).
        self.tags = {}
        self.accepted = False
        g = lua.globals()
        g.Find = lambda k: self.tags.get(k, '')
        g.Holds = lambda k: k in self.tags
        g.Accept = self._accept
        g.Id = lambda: self.rel_id
        lua.execute(open(path).read())
        self.way_keys = self._keys(g.way_keys)
        self.node_keys = self._keys(g.node_keys)
        self.scan = g.relation_scan_function
        src = open(path).read()
        empty_node_fn = re.search(r'^function node_function\s*\([^)]*\)\s*end\b', src, re.M)
        self.needs_tagged_nodes = self.node_keys is None and g.node_function is not None and not empty_node_fn

    def _accept(self):
        self.accepted = True

    @staticmethod
    def _keys(t):
        if t is None:
            return None
        return [t[i] for i in range(1, len(t) + 1)]

    def accepts(self, rel_id, tags):
        """tilemaker's relation scan: multipolygons by way_keys, the rest by
        relation_scan_function."""
        if tags.get('type') == 'multipolygon':
            return False  # handled by the tag filter, below
        if self.scan is None:
            return False
        self.tags, self.rel_id, self.accepted = tags, rel_id, False
        self.scan()
        return self.accepted


def expressions(profiles):
    """osmium tags-filter expressions: ways and multipolygons by way_keys,
    nodes by node_keys."""
    out = []
    for p in profiles:
        if p.way_keys is None:
            out.append('w/*')
        for k in p.way_keys or []:
            if k.startswith('~'):
                sys.exit(f'{p.name}: negated way_keys ({k}) not supported by this prototype')
            out += [f'w/{k}', f'r/{k}']
        if p.needs_tagged_nodes:
            out.append('n/*')
        for k in p.node_keys or []:
            out.append(f'n/{k}')
    return sorted(set(out))


def main(src, dst, *paths):
    profiles = [Profile(p) for p in paths]
    for p in profiles:
        print(f'{p.name}: way_keys={p.way_keys} node_keys={p.node_keys} '
              f'scan={"yes" if p.scan else "no"} tagged_nodes={p.needs_tagged_nodes}', file=sys.stderr)
    accepted = set()
    for r in osmium.FileProcessor(src, osmium.osm.RELATION):
        tags = {t.k: t.v for t in r.tags}
        if any(p.accepts(r.id, tags) for p in profiles):
            accepted.add(r.id)
    print(f'{len(accepted)} relations accepted by relation_scan_function', file=sys.stderr)
    exprs = expressions(profiles)
    with tempfile.TemporaryDirectory() as tmp:
        parts = []
        if exprs:
            parts.append(f'{tmp}/tags.osm.pbf')
            subprocess.run(['osmium', 'tags-filter', '-O', '-o', parts[-1], src, *exprs], check=True)
        if accepted:
            ids = f'{tmp}/ids.txt'
            with open(ids, 'w') as f:
                f.writelines(f'r{i}\n' for i in sorted(accepted))
            parts.append(f'{tmp}/rels.osm.pbf')
            # Exits 1 if some members aren't in the input (an extract's
            # relations reach outside it); it still writes what it found.
            r = subprocess.run(['osmium', 'getid', '-O', '-r', '-i', ids, '-o', parts[-1], src])
            if r.returncode not in (0, 1) or not os.path.exists(parts[-1]):
                sys.exit(f'osmium getid failed ({r.returncode})')
        if not parts:
            # The profiles read nothing from the PBF (e.g. only GeoJSON).
            parts.append(f'{tmp}/empty.osm.pbf')
            subprocess.run(['osmium', 'tags-filter', '-O', '-o', parts[-1], src, 'n/pbf-filter-matches-nothing'], check=True)
        if len(parts) == 1:
            os.replace(parts[0], dst)
        else:
            subprocess.run(['osmium', 'merge', '-O', '-o', dst, *parts], check=True)
    print(f'{dst}: {os.path.getsize(dst) / 1e6:.1f} MB of {os.path.getsize(src) / 1e6:.1f} MB', file=sys.stderr)


if __name__ == '__main__':
    if len(sys.argv) < 4:
        sys.exit(__doc__)
    main(*sys.argv[1:])
