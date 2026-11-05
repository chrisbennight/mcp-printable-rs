# TRELLIS.2 disk-streaming measurements

Printable's optional TRELLIS.2 shape workflow completed the workloads below
with less than 6 GB of sampled process GPU memory. These are measurements on
an allocator-constrained 16 GB card, not qualification on a physical 6 GB GPU.
The generated surfaces require inspection and repair before slicing.

## Release and hardware

The measured application source is
[`1280774529a9e4801b7a5ef72be6d89b1adea322`](https://github.com/chrisbennight/mcp-printable-rs/commit/1280774529a9e4801b7a5ef72be6d89b1adea322).
The [measurement record](trellis2-low-vram.json) contains immutable release
and reconstruction-image references, input hashes and unrounded results.
All matching container images were published and their manifests and source
labels were verified through anonymous registry access.

The host ran Linux/x86_64, kernel 7.0.0-38-generic, on an Intel Core i5-12600H,
with 100,962,488,320 bytes of host RAM. The GPU was an NVIDIA GeForce RTX 4060 Ti
reporting 16,380 MiB, with driver 580.178.04. Model and scratch storage used
disk-backed volumes on NVMe.

The worker had 8 CPUs, a 10 GiB cgroup memory limit and no additional cgroup
swap allowance. The PyTorch allocation budget was 4,096 MiB. This budget
does not limit driver allocations. No host swap settings were changed.

## Workloads and results

The source picture is the upstream
[T.png fixture](https://github.com/microsoft/TRELLIS.2/blob/75fbf0183001ed9876c8dbb35de6b68552ee08bd/assets/example_image/T.png),
a 1024 by 1024 RGBA image. The RGB case composites its alpha onto white.
All TRELLIS.2 cases request resolution 1536, seed 42, 12 sampling steps per
stage and a longest side of 100 mm. The compact choice targets 1,000,000
triangles; full detail omits simplification.

| Case | Elapsed | Sampled driver GPU | Process RAM | Generated triangles | Exported triangles |
| --- | ---: | ---: | ---: | ---: | ---: |
| RGBA, compact | 16m 53s | 4.22 GB | 6.06 GB | 25,575,950 | 1,000,000 |
| RGBA, full detail | 14m 03s | 4.20 GB | 6.06 GB | 24,560,764 | 24,560,764 |
| RGB, compact | 17m 41s | 4.20 GB | 6.11 GB | 25,733,710 | 1,000,000 |

GB in this report means 1,000,000,000 bytes. GiB means 1,073,741,824 bytes.
The prepared model cache existed before these successful runs. The first
successful job is not a first-use download benchmark or a controlled cold-cache
measurement. Model preparation from unprepared storage and reuse after container
replacement were checked separately.

The repeated RGBA input and seed did not produce identical geometry counts.
The measurements establish successful execution and per-job output retention;
they do not establish deterministic meshes. Different generation and delivery
times also prevent attributing the total-time difference to caching alone.

## What was measured

The retained job result records process resident-memory high-water marks,
PyTorch allocated-memory peaks, process-stage times and inference-stage times.
The process RAM column is the greatest reported stage-process high-water mark,
not host RAM consumption. A separate observer sampled driver-attributed GPU
memory, summed process RSS, cgroup memory and cgroup block I/O approximately
every 250 ms for the selected worker and its children. The record includes
sample counts, largest gaps and endpoint coverage. Sampling can miss brief
peaks, and summed RSS can count shared pages more than once.

The compact RGBA job used 225,806,360,576 bytes of physical NVMe reads and
2,889,596,928 bytes of writes in its observed I/O window. The full-detail job
used 224,361,897,984 bytes of reads and 1,257,623,552 bytes of writes. The logical
volume and physical device report the same traffic; only the physical-device
counter is counted. The RGB case used 224,723,492,864 bytes of reads and
2,710,990,848 bytes of writes. I/O endpoints are the observer's sample boundaries
near job admission and completion, rather than exact per-operation attribution.

Container memory includes reclaimable file cache and reached the 10 GiB limit
during the full-detail case. Successful execution at that limit does not prove
that less container RAM is sufficient, or that 10 GiB is a minimum. The host
had substantially more RAM than the container. These results do not qualify
a small-RAM host or a slow disk.

## Artifact and workflow checks

The compact RGBA binary STL contains 1,000,000 triangles in 50,000,084 bytes.
It was published, streamed to a separate client and checksum-verified.
An independent bounded-buffer reader checked its binary structure, finite
coordinates, triangle count, centering, Z=0 grounding and 100 mm scale.

The full-detail STL contains all 24,560,764 triangles from its own generation
in 1,228,038,284 bytes. A separate bounded-buffer reader checked that file
where it was stored, including its complete SHA-256, finite coordinates,
binary length, count and scale. Transfer of this file through the downstream
gateway was refused by that gateway's file-size quota; this run does not
establish end-to-end gateway transfer for artifacts above that quota.
The compact transfer succeeded. Operators need sufficient transfer-service
capacity as well as workspace storage for full-detail outputs.

Printable's geometry validator found that the compact RGBA mesh was not
watertight, consistently oriented or manifold. It reported 111 connected
components and 21,165 duplicate faces. Its mesh assessment failed. Generation
and simplification do not repair topology, qualify wall thickness, prove
slicing success or establish physical print quality. Follow the
[inspection and repair workflow](../docs/photo-object.md#from-the-mesh-to-a-print).

## Reproduce and interpret

Use the matching published release with the
[photograph-worker overlay](../compose.photo.yaml) and
[persistent model and scratch settings](../docs/photo-object.md#persistent-models-and-scratch-storage).
Allow disk capacity for model data, scratch, final artifacts and publication
copies. The manifest requires 10,506,698,875 bytes of pinned model data.
First use obtains missing files from Hugging Face; verified byte-identical
dependencies may also be reused from an existing Pixal3D model mount.

Place the fixture in the workspace and submit the following through `photo`,
using a new output path for each case:

```json
{"action":"object","params":{"model":"trellis2","image":"photos/T.png","mesh_path":"objects/T-compact.stl","resolution":1536,"seed":42,"size_mm":100,"simplify":true}}
```

Retain the returned job identifier and query `status` with `detail: true`
until a terminal state. For full detail use `simplify: false` and omit
`target_triangles`. For the RGB case use a white-background RGB composite.
Record the input hash, requested and achieved settings, immutable image
identities, GPU model/driver and actual cgroup limits. Start an independent
driver/cgroup observer before admission and retain its complete sample window
through terminal status. Read physical block-device counters once per window
without also adding their logical-volume aliases. Validate the output
separately from successful job status.

Software regression coverage includes
[model preparation and offline reuse](../recon/tests/test_model_cache.py),
[weight streaming](../recon/tests/test_trellis_memory.py),
[stage isolation and result handling](../recon/tests/test_trellis_cli.py), and
[mesh delivery](../recon/tests/test_mesh_delivery.py). CPU tests establish
these contracts; the GPU measurements above establish only the listed
hardware/input/profile coverage.
