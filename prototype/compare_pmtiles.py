# Compares two PMTiles archives tile by tile (decompressed bytes).
import gzip, json, sys, zstandard
from collections import Counter
import mapbox_vector_tile as mvt
from pmtiles.reader import MmapSource, Reader, all_tiles

def tiles(path):
    f = open(path, 'rb')
    out = {}
    for (z, x, y), data in all_tiles(MmapSource(f)):
        out[(z, x, y)] = gzip.decompress(data) if data[:2] == b'\x1f\x8b' else (zstandard.ZstdDecompressor().decompress(data, max_output_size=1<<28) if data[:4] == b'\x28\xb5\x2f\xfd' else data)
    return out

a, b = tiles(sys.argv[1]), tiles(sys.argv[2])
only_a = a.keys() - b.keys(); only_b = b.keys() - a.keys()
def feats(data):
    d = mvt.decode(data)
    return {name: Counter(json.dumps([f['geometry'], f['properties']], sort_keys=True) for f in layer['features']) for name, layer in d.items()}
diff = [k for k in a.keys() & b.keys() if a[k] != b[k] and feats(a[k]) != feats(b[k])]
print(f'{len(a)} vs {len(b)} tiles; only in first {len(only_a)}, only in second {len(only_b)}, differing features {len(diff)}')
for k in sorted(diff)[:5]: print('  differs', k, len(a[k]), len(b[k]))
for k in sorted(only_a)[:3]: print('  only first', k, len(a[k]))
for k in sorted(only_b)[:3]: print('  only second', k, len(b[k]))
