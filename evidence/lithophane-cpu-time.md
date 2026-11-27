# Lithophane time on the CPU

Date: 2026-11-27. Host: a `rust:1.97.1-trixie` Linux container on an Apple
silicon Mac (`aarch64`, 8 CPUs visible to the container), not the supported
Linux/amd64 installation. Build: the release `printable-geometry-worker` from
this branch, with linear-light brightness before resizing, borders and a
fine flat face on curved plates, and the STL written as it is encoded.

The input was a synthetic 1024 x 1024 pixel 8-bit RGB PNG with smooth
gradients, rings and fine stripes. Each run started the worker as the server
does, `printable-geometry-worker --lithophane <request.json> <output>`, with
the default settings (0.8 to 3 mm, `white_pla`, 0.4 mm nozzle, no border).
`/usr/bin/time` measured the whole process, from decoding to writing the STL
and preview, and its peak resident memory. Each case ran three times.

| Case | Samples | Triangles | STL bytes | Seconds (three runs) | Peak memory | `checks.passed` |
| --- | --- | --- | --- | --- | --- | --- |
| flat, 100 mm wide | 501 x 501 | 508,168 | 25,408,484 | 0.23, 0.21, 0.21 | 30 MiB | true |
| curved, 100 mm wide, 120 degrees | 501 x 501 | 1,004,000 | 50,200,084 | 0.41, 0.42, 0.40 | 40 MiB | true |
| flat, 200 mm wide | 1001 x 1001 | 2,020,182 | 101,009,184 | 0.84, 0.83, 0.83 | 93 MiB | true |

The work follows the number of samples, which the size and nozzle set, more
than the image's pixel count. Times and memory on an amd64 host were not
measured.
