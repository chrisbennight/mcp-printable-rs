# Assembly input memory

Assembly analysis streams both accepted STL inputs into immutable workspace
snapshots and passes those snapshot paths to the isolated geometry worker. The
worker reads and expands the meshes under its configured address-space limit.
The server does not retain both complete model byte buffers or create a second
temporary model pair.

## Reproduce the measurements

Use Linux, the checked-in Rust toolchain, and Python 3. Ordinary fake-backed
tests need no Blender, GPU, printer, or deployed service.

```sh
cargo test -p printable-server --test assembly_memory_test --locked -- --nocapture
cargo test -p printable-server --test assembly_analysis_test --locked -- --nocapture
cargo test -p printable-server --test tools_test --locked assembly_
```

These tests print machine-readable records prefixed with
`ASSEMBLY_MEMORY_EVIDENCE=` and `ASSEMBLY_ANALYSIS_EVIDENCE=`. Retain those records
with the exact source revision and any uncommitted production diff, toolchain,
container image digest, CPU architecture, build profile, and exit status. When
sharing a Cargo target directory between source candidates, refresh the
`printable-server` package before each build so stale fingerprints cannot
substitute a previously built server.

To compare the historical baseline, archive revision
`2e46398fb11bd99cc58dac256b82848e46b6eb0a` and copy these two measurement test
files into its `crates/printable-server/tests/` directory without changing its
production source. Its preparation test is expected to fail after printing
both size records. Run its native measurements with
`--exact increasing_tessellation_preserves_real_worker_measurements --nocapture`
after Cargo's `--` separator. Retain the copied test hashes with the results.

## Workloads and measurement boundaries

The large-file preparation test uses valid binary STL files containing disjoint
closed tetrahedra, with approximately 8 MiB and 64 MiB per input. Each size runs
in a fresh server test process. A small file-inspecting worker deliberately
avoids mesh parsing and constructive solid geometry (CSG). It checks immutable
input bytes after replacing the original public filenames and measures temporary model
bytes, its own Linux `VmHWM`, and inspection time. The inspection includes a
fixed wait that lets an unrelated status request complete. Its duration is
**synthetic inspection time**, not geometry analysis time. Total elapsed time
minus inspection time includes snapshot preparation, worker startup, Python
imports, orchestration, and result delivery; it is not a precise snapshot-only
timer.

The real-worker test uses two closed 1 mm cubes with increasingly tessellated
faces: 48, 768, and 12,288 triangles per input. One arrangement has a 1 mm
clearance and no interference; another overlaps by 0.5 mm and has an expected
interference volume of 0.5 mm³. Each input cube must retain a volume of 1 mm³.
Both arrangements run three trials per tessellation in fresh server test
processes. Only one geometry call is active in each trial.

A test-only Python wrapper invokes the actual `printable-geometry-worker`
binary with the unchanged snapshot paths and options. It inherits the server's
configured 1 GiB `RLIMIT_AS` and records that value. Native worker process time
includes executable startup, input reads, STL decoding, geometry preparation,
exact analysis, report serialization, and process exit. Server preparation and
process overhead is total tool-call time minus that interval; it also includes
the instrumentation wrapper's startup and result processing. The worker's
maximum resident set size comes from `RUSAGE_CHILDREN` in a fresh wrapper
process with one native child. This measures the largest waited child's
resident memory, rather than the combined process tree. It is distinct from
the wrapper's own `VmHWM` used by the large-file test. The child lifetime also
includes memory inherited before executable replacement. Values near 10 MiB
in the smaller native workloads can therefore reflect memory inherited from
the wrapper; they are not precise allocation peaks after the native binary
starts.
Resource limits persist
across process creation and executable replacement, so the native worker
inherits the wrapper's limit. See the Linux manuals for
[resource usage](https://man7.org/linux/man-pages/man2/getrusage.2.html) and
[resource limits](https://man7.org/linux/man-pages/man2/getrlimit.2.html).

Server memory is Linux `/proc/self/status` `VmHWM` before and after the tool
call. This is an approximate kernel high-water counter, not a sampled
allocation trace; the Linux [process status manual](https://man7.org/linux/man-pages/man5/proc_pid_status.5.html)
documents its accounting limitations. Input generation occurs before the
interval. Temporary input bytes count immutable
STL snapshots and any additional model copies at worker admission, without
counting the two retained public inputs, options, or filesystem metadata.

## Correctness and ownership

The preparation regression requires server peak growth below 16 MiB for both
large-file sizes and exactly one temporary snapshot pair. These are regression
thresholds for the test workload, not production model-size limits. The
baseline that buffers whole inputs is expected to fail those assertions.

The real-worker regression verifies the known cube volumes, clearance, and
interference results at every tessellation and trial. Existing assembly tests
cover invalid solids, missing workers, motion analysis, and a worker that cannot
start within a deliberately small address-space budget. Failure must preserve
the original artifact and release temporary snapshots.

The cancellation regression pauses an isolated worker, cancels its requesting
future, and verifies that snapshot cleanup still reports the inputs as
protected. A second analysis must wait for the geometry lane until the first
worker exits. Once released, the worker verifies its input files still exist;
the second call completes and all snapshots are reclaimed. This proves owned
blocking work outlives cancellation of its waiter without making its files or
geometry capacity available prematurely.

## Evidence scope

The complete [measurement records](assembly-input-memory.json) retain all four
large-file samples, all 36 native-worker trials, the expected baseline
regression failure, source hashes, timing summaries, and environment details.
The preparation baseline and candidate were measured from revision
`2e46398fb11bd99cc58dac256b82848e46b6eb0a`, with the snapshot-path change applied
only to the candidate. The native-worker candidate used parent revision
`ac86e5f7bd666577723f1ae69edde3c6b19ba89d` with the same production change. The
geometry-worker source, assembly implementation, lockfile, and measurement
tests have identical recorded hashes in the two source sets.

The local container ran Linux/arm64, kernel `6.8.0-117-generic`, Python 3.11.2,
Rust 1.97.1, and the unoptimized test profile with debug information. It exposed
eight CPUs; the CPU model was unavailable. No GPU was used.
The Docker VM memory budget was 12,514,275,328 bytes, with no per-container
memory cap configured. The geometry worker's separate 1 GiB address-space
limit still applied. The local image ID
was `sha256:7f86f402306e669aaa302392b72df885cf854c013eee4fbb08dfd6afd89d6f55`.
It was built from
`rust@sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97`
with CMake and Python venv support installed. These are measurement image
identities, not published installation images.

Baseline and candidate batches ran sequentially, without CPU pinning, control
of other workloads, or randomized order. Their input filesystems also differed:
the old worker read additional model copies from the container's `/tmp`, while the
candidate read immutable snapshots from a workspace bind mount shared from
macOS. This storage difference can contribute to native-worker input I/O time;
it is a plausible confound rather than a confirmed explanation of the timing
differences below.

The records establish software behavior and memory ownership for these
workloads. They do not qualify Linux/amd64 latency on a native filesystem,
NVIDIA performance, pure CSG kernel time, or arbitrary assemblies. Production
latency needs separate Linux/amd64 filesystem qualification.

## Observed results

The large-file regression failed on the buffering baseline and passed on the
snapshot-path candidate. Each row below is one trial. MiB values are rounded;
the JSON retains exact input sizes and counters.

| Input MiB per part | Source | Server peak growth MiB | Temporary model MiB | Inspection worker peak MiB | Preparation/process ms | Synthetic inspection ms |
| --- | --- | --- | --- | --- | --- | --- |
| 8 | baseline | 21.95 | 32.00 | 9.26 | 112.2 | 210.6 |
| 8 | candidate | 6.38 | 16.00 | 9.28 | 83.4 | 215.0 |
| 64 | baseline | 133.94 | 256.00 | 9.28 | 358.6 | 233.6 |
| 64 | candidate | 6.38 | 128.00 | 9.28 | 251.0 | 222.4 |

Increasing each input from approximately 8 MiB to 64 MiB increased the
baseline server's recorded peak growth from 21.95 MiB to 133.94 MiB. Candidate
peak growth stayed at 6.38 MiB, and temporary model bytes fell from four times
one input's size to twice that size. The single-trial preparation timings are
observations, not a general speedup claim.

Both real-worker suites passed all geometry assertions. Each timing cell
below shows its own median and `[minimum, maximum]` across three trials, in
milliseconds. Native worker process time includes input I/O and exact geometry
analysis; preparation/process overhead includes the test wrapper.

| Triangles per cube | Arrangement | Source | Preparation/process overhead ms | Native worker process ms | Total tool ms |
| --- | --- | --- | --- | --- | --- |
| 48 | separated | baseline | 28.7 [27.2, 50.3] | 5.6 [5.5, 17.5] | 34.3 [32.7, 67.8] |
| 48 | separated | candidate | 63.3 [48.9, 80.4] | 11.9 [11.6, 16.0] | 74.9 [60.8, 96.4] |
| 48 | overlapping | baseline | 27.0 [26.8, 27.7] | 5.6 [5.4, 5.6] | 32.4 [32.3, 33.2] |
| 48 | overlapping | candidate | 49.5 [46.6, 54.3] | 12.7 [12.4, 13.4] | 62.3 [59.0, 67.8] |
| 768 | separated | baseline | 27.7 [27.3, 28.1] | 27.1 [26.5, 27.8] | 54.6 [54.4, 55.5] |
| 768 | separated | candidate | 46.6 [45.4, 52.0] | 65.4 [63.3, 67.2] | 113.7 [108.6, 117.4] |
| 768 | overlapping | baseline | 28.1 [27.8, 28.9] | 27.9 [27.8, 28.2] | 56.3 [55.6, 56.8] |
| 768 | overlapping | candidate | 55.3 [53.1, 61.5] | 75.6 [72.8, 85.9] | 131.0 [125.9, 147.4] |
| 12,288 | separated | baseline | 30.7 [30.0, 30.7] | 407.9 [407.4, 408.2] | 438.1 [437.9, 438.9] |
| 12,288 | separated | candidate | 54.2 [45.3, 73.6] | 761.1 [740.5, 777.3] | 815.2 [785.8, 850.9] |
| 12,288 | overlapping | baseline | 30.3 [29.7, 30.3] | 415.0 [414.6, 425.6] | 445.3 [444.2, 455.9] |
| 12,288 | overlapping | candidate | 54.4 [52.0, 64.3] | 794.9 [789.7, 816.7] | 849.3 [841.7, 880.9] |

Native and total times were higher in the candidate batch, including the
largest separated case's median of 761.1 ms versus 407.9 ms in the worker and
815.2 ms versus 438.1 ms for the whole tool call. The experiment retains this
slower result and does not establish a causal speedup or regression on the
supported deployment filesystem.

The real-worker memory and temporary-input ranges below span both arrangements
and all three trials. Every worker inherited the configured 1 GiB address-space
limit. Server peak growth stayed below 6.4 MiB throughout the candidate native
trials.

| Triangles per cube | STL bytes per cube | Baseline temporary bytes | Candidate temporary bytes | Baseline child peak KiB | Candidate child peak KiB |
| --- | --- | --- | --- | --- | --- |
| 48 | 2,484 | 9,936 | 4,968 | 10,112–10,240 | 10,240 |
| 768 | 38,484 | 153,936 | 76,968 | 10,240 | 10,240 |
| 12,288 | 614,484 | 2,457,936 | 1,228,968 | 22,688–22,692 | 22,576–22,688 |

For every trial, both cube volumes were 1 mm³. Separated cubes had 1 mm
clearance and zero interference; overlapping cubes had zero clearance and
0.5 mm³ interference. Temporary snapshots were reclaimed after successful
analysis, and the candidate's separate cancellation ownership test passed.
