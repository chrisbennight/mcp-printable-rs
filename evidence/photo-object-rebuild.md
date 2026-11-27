# Rebuilding generated photo objects

Date: 2026-11-27. Inputs: three raw surfaces that Pixal3D generated at
`photo.object` defaults (`raw_surface: true`, 1,000,000 triangles each, longest
side 100 mm) from photographs of a marble bust, a bicycle and a cluttered chess
set. The surfaces are not included: they are 50 MB each and come from
third-party photographs. Before this change every one failed the default
rebuild: the bust with "the extracted surface is invalid: NotManifold", the
bicycle and the chess set by exceeding the geometry worker's 1 GiB address
space.

Each surface was passed to the release `printable-geometry-worker
--prepare-print` with the request the photograph worker sends at defaults:
only the largest part kept, longest side 100 mm, overhangs checked at 45
degrees and walls at 0.4 mm, under `ulimit -v 1048576` (1 GiB). The host was an
aarch64 Linux container (Debian trixie, `rust:1.97.1-trixie`); the table gives
the median of three runs. The same binary cross-built for x86-64 and run in an
emulated amd64 container gave the same meshes, with the peak memory shown in
the last column; its times are not comparable.

| Surface | Source shells / open edges | Result triangles | Facet edge | Result size (mm) | Wall s | CPU s | Peak memory | Peak memory, x86-64 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Marble bust | 20,847 / 21,472 | 420,988 | 0.40 mm | 64.0 × 60.6 × 100.0 | 3.7 | 5.0 | 407 MiB | 548 MiB |
| Bicycle | 141,853 / 84,198 | 61,312 | 0.43 mm | 100.0 × 26.8 × 76.6 | 3.9 | 4.7 | 391 MiB | 430 MiB |
| Chess set | 13,108 / 15,942 | 177,116 | 0.40 mm | 37.5 × 44.3 × 100.0 | 1.4 | 1.7 | 181 MiB | 278 MiB |

Every result was one shell and passed the strict check: a valid solid with no
self-intersections and no shells touching or crossing. "Source shells" counts
pieces joined through edges two triangles use; the generator's surfaces have
many edges used by three or more triangles and many triangles facing the wrong
way, which split them into many such pieces.

What the results show:

- The bust's rebuilt surface lies within 0.19 mm of the raw surface at the
  95th percentile of its vertices, and the raw surface within 0.11 mm of the
  rebuilt one, measured without rescaling. About 1% of the rebuilt surface lies
  1 to 9 mm from the raw one, at heights between 21 and 34 mm, most likely
  where it spans openings in the raw surface.
- Only the largest part is kept, so the chess set's result is one piece, the
  largest, rescaled to 100 mm; the other pieces are dropped before the rebuild.
- At the default facet size the bicycle's thinnest parts break away and are
  removed with the other pieces: the kept part is 94 mm long before rescaling,
  against the generated 100 mm, and 1,308 mm³ in volume. With `edge_length_mm: 0.2` the kept
  part is 99 × 44 × 73 mm, in 36 seconds and 497 MiB, still within the 1 GiB
  limit.
