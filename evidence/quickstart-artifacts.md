# Tutorial artifact evidence

A local isolated installation run completed the supplied bracket tutorial on
11 September 2026. It used the installation candidate's server and matching
Blender sources with software graphics. The direct HTTP client verified each
download's declared size and SHA-256 before accepting it.

| Download | Observed bytes |
| --- | ---: |
| STL | 42,758 |
| Dimensioned PNG | 166,358 |
| Blender checkpoint | 99,804 |
| MP4 | 4,456 |

The video job completed its eight requested frames. The
[dimensioned PNG](../docs/images/quickstart-bracket.png) is the actual downloaded
output used in the README. It shows the bracket's front, right, and top views
with the expected 40 × 30 × 24 mm overall dimensions.

These values describe that run, not guaranteed sizes or performance budgets.
No controlled latency, host-memory, VRAM, or NVIDIA throughput measurement was
collected. The Python direct client and the Rust MCP smoke are independently
implemented protocol clients; the separate container corpus also tests live
editing while a checkpoint render occupies the isolated worker. This does not
claim compatibility with every third-party MCP client.

Reproduce the workflow with the commands in the
[installation guide](../docs/installation.md). Keep its JSON validation, scene,
job, and artifact records alongside the downloaded files when assessing another
machine or revision.
