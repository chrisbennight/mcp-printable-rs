# Lithophane time on the CPU

Date: 2026-11-26. Host: a `rust:1.97.1-trixie` Linux container on an Apple
silicon Mac (`aarch64`, 8 CPUs visible to the container), not the supported
Linux/amd64 installation. Build: the release `printable-geometry-worker` from
this branch.

The input was a synthetic 1024 x 1024 pixel 8-bit RGB PNG with smooth
gradients, rings and fine stripes. Each run started the worker as the server
does, `printable-geometry-worker --lithophane <request.json> <output>`, with
the default settings (0.8 to 3 mm, `white_pla`, 0.4 mm nozzle, no border), and
was timed from the shell around the whole process: decoding, resampling, tone
mapping, the solid, its checks, and writing the STL and preview. Each case ran
three times.

| Case | Samples | Triangles | STL bytes | Seconds (three runs) | `checks.passed` |
| --- | --- | --- | --- | --- | --- |
| flat, 100 mm wide | 501 x 501 | 508,168 | 25,408,484 | 0.066, 0.074, 0.076 | true |
| curved, 100 mm wide, 120 degrees | 501 x 501 | 508,168 | 25,408,484 | 0.067, 0.062, 0.063 | true |
| flat, 200 mm wide | 1001 x 1001 | 2,020,182 | 101,009,184 | 0.242, 0.217, 0.231 | true |

The work follows the number of samples, which the size and nozzle set, not the
image's pixel count. Times on an amd64 host were not measured.
