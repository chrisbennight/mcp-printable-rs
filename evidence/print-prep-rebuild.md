# Rebuilding a one-million-triangle mesh for printing

Date: 2026-11-27. Host: an aarch64 Linux container on an 8-core machine (Debian trixie,
`rust:1.97.1-trixie`) on a development machine, processor only. Binary: the
release `printable-geometry-worker` built from this branch. These figures show
the order of cost on a processor; the published amd64 images and other
processors differ.

The input is the script below: a lumpy closed surface 107 mm tall with
1,000,000 triangles, a 20 mm block overlapping it and a 3 mm cube beside it.
The lumpy surface faces inwards, as some exporters write one, and has five
openings: three missing quadrilaterals and the two poles, whose triangles are
degenerate. The worker read it, rebuilt it, ran the strict check and wrote the
rebuilt STL with `printable-geometry-worker --prepare-print request.json
out.stl`, where the request held the source path, `{"checks":
{"overhang_degrees": 45, "min_feature_mm": 0.4}}`, an overhang angle of 45
degrees and the subject name; and once more with `"edge_length_mm": 0.2`. This
is the whole worker run behind `scene` `prepare_print`, including the
thin-wall check against 0.4 mm and the strict validation of the written mesh.
The container was limited to two processors. `/usr/bin/time -v` measured each run three
times; the table gives the median.

| Edge | Address-space limit | Output triangles | Wall seconds | CPU seconds (user + system) | Peak resident memory |
| --- | --- | --- | --- | --- | --- |
| 0.48 mm (default, 0.4% of the longest side) | 1 GiB | 507,332 | 3.2 | 3.6 | 491 MiB |
| 0.2 mm | none | 2,945,256 | 15.4 | 16.7 | 2,499 MiB |
| 0.2 mm | 1 GiB | none: the worker was stopped when it ran out of address space | | | |

Both runs closed the five openings, turned the inward-facing surface to face
outwards (`cavities_filled: 1`), merged the overlapping block into it and kept
the floating cube, which the report lists as one floating part, since every
part is kept by default. Each result was two shells, a valid solid with no
self-intersections and no shells touching or crossing; the thin-wall check
found 159 mm² thinner than 0.4 mm on the lumpy surface. The default edge fits within the geometry worker's
default 1 GiB limit. Finer edges need more memory: raise
`PRINTABLE_GEOMETRY_WORKER_MEMORY_MIB` for them. Measured values are in
[print-prep-rebuild.json](print-prep-rebuild.json).

```python
# A lumpy closed surface of about 1,000,000 triangles, 100 mm tall, standing
# on z = 0, with a few holes punched in it, a floating speck and a duplicated
# overlapping lobe, written as binary STL.
import math, struct, sys
n_lat, n_lon = 500, 1000          # 2 * 500 * 1000 = 1,000,000 triangles
def radius(theta, phi):
    return 40 * (1 + 0.15 * math.sin(5 * phi) * math.sin(3 * theta) + 0.05 * math.cos(17 * phi + 11 * theta))
pts = []
for i in range(n_lat + 1):
    t = math.pi * i / n_lat
    row = []
    for j in range(n_lon):
        p = 2 * math.pi * j / n_lon
        r = radius(t, p)
        row.append((r * math.sin(t) * math.cos(p), r * math.sin(t) * math.sin(p), 50 - r * math.cos(t) * 1.2))
    pts.append(row)
tris = []
holes = {(100, 200), (300, 600), (250, 10)}
for i in range(n_lat):
    for j in range(n_lon):
        if (i, j) in holes:
            continue
        a, b = pts[i][j], pts[i][(j + 1) % n_lon]
        c, d = pts[i + 1][j], pts[i + 1][(j + 1) % n_lon]
        tris.append((a, c, b)); tris.append((b, c, d))
def cube(o, s):
    v = [(o[0] + s * (k & 1), o[1] + s * (k >> 1 & 1), o[2] + s * (k >> 2 & 1)) for k in range(8)]
    for f in [(0,2,1),(1,2,3),(4,5,6),(5,7,6),(0,1,4),(1,5,4),(2,6,3),(3,6,7),(0,4,2),(2,4,6),(1,3,5),(3,7,5)]:
        tris.append(tuple(v[k] for k in f))
cube((70, 0, 80), 3)
cube((30, -5, 30), 20)
lo = min(p[2] for t in tris for p in t)
with open(sys.argv[1], "wb") as out:
    out.write(b"\0" * 80 + struct.pack("<I", len(tris)))
    for t in tris:
        out.write(struct.pack("<3f", 0, 0, 0))
        for p in t:
            out.write(struct.pack("<3f", p[0], p[1], p[2] - lo))
        out.write(b"\0\0")
print(len(tris))
```
