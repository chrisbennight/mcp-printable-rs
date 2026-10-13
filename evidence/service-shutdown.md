# Service shutdown evidence

Normal Docker stops entered the graceful shutdown path for the candidate server,
CAD, reconstruction, and slicer services. Accepted synchronous CAD work completed
before exit. Background and forcibly stopped work retained unfinished state and
reported `interrupted` after restart, without replay. A graceful service exit did
not imply that background work completed.

The [complete raw records](service-shutdown.json) contain 13 completed Docker
probes and 4 restart observations, captured at `2026-10-12T23:48:44.277862+00:00`.
Every case used a new disposable workspace and fake native engine. No physical
printer, user workspace, Blender mutation, or GPU workload was contacted.

The frozen baseline was `2e46398fb11bd99cc58dac256b82848e46b6eb0a` (tree
`9544aa289fb67e164b46cde6f701a0e91153a447`). The candidate was an uncommitted change on
`ac86e5f7bd666577723f1ae69edde3c6b19ba89d`. Its measured runtime subtree at
`crates/printable-server` is `f26b387ea26e743977b217b7e423be8bbdaca9ed`. The JSON
records every measured entrypoint, signal helper, regression source, and lockfile
by Git blob and SHA-256, together with all five frozen binary hashes and sizes.
The baseline and candidate parent had identical service entrypoints and
CAD/reconstruction/slicer implementations; the candidate adds the shared signal
handler and regression coverage. The baseline archive had a test-only admission
regression overlay, with production files unchanged.

These are Linux/arm64 development binaries with debug information in a local
software-only toolchain image, rather than the supported Linux/amd64 NVIDIA
release image set. The image is `sha256:7f86f402306e669aaa302392b72df885cf854c013eee4fbb08dfd6afd89d6f55`, based on
`rust@sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97` with CMake and Python tooling. The run used
`rustc 1.97.1 (8bab26f4f 2026-07-14)`, Python 3.11.2, Linux kernel
6.8.0-117-generic, Docker Engine 29.5.2, and
8 visible CPUs. The Docker VM memory budget was
12,514,275,328 bytes, with no per-container memory cap.
No GPU performance or real-engine completion is established by these results.

Containers used Docker's `--init`, a non-root UID/GID, dropped capabilities,
`no-new-privileges`, loopback-only dynamic ports, read-only frozen binaries, and
one disposable fixture mount. They had no Docker socket or host PID namespace.
The CAD fixture recorded its start, waited for an explicit release, and wrote
fake artifacts only after that release. Each stop issued `SIGTERM`; grace periods
were 5 seconds for idle/background work, 8 seconds for synchronous work, and
1 second for the forced case.

Idle observations, with a 5-second grace period:

| Service | Baseline exit / stop ms | Candidate exit / stop ms |
| --- | --- | --- |
| `printable-server` | 143 / 157.1 | 0 / 136.3 |
| `printable-cad-worker` | 143 / 135.9 | 0 / 134.6 |
| `printable-recon-worker` | 143 / 140.4 | 0 / 145.0 |
| `printable-slicer-worker` | 143 / 126.3 | 0 / 126.2 |

Every baseline idle stop exited 143 without the drain log. Every candidate idle
stop exited zero and logged shutdown. All observed stopped containers had
`Running=false`, `Pid=0`, and `OOMKilled=false`.

Active CAD observations:

| Source / CAD work | Grace s | Exit | Stop ms | Retained state after stop | Restart status |
| --- | ---: | ---: | ---: | --- | --- |
| baseline / foreground | 8 | 143 | 145.3 | running | not probed |
| baseline / background | 5 | 143 | 141.2 | running | interrupted |
| candidate / foreground | 8 | 0 | 179.0 | completed | completed |
| candidate / background | 5 | 0 | 127.0 | running | interrupted |
| candidate / forced foreground | 1 | 137 | 1158.0 | running | interrupted |

The baseline synchronous request lost its connection and produced no completed
report. The candidate held the accepted request open, rejected a new connection
while draining, and completed after the fixture was released. Its request,
snapshotted source, report, and model bytes remained unchanged after restart;
status remained `completed` and the engine-start count remained one.

Both background admissions returned `admitted`. Their retained files still said
`running` after stop, but restarted services exposed `interrupted`, preserved
request/source bytes, produced no completed report, and did not start the engine
again. The candidate background service exited zero despite unfinished work.
The forced synchronous case first entered shutdown, then exceeded its 1-second
grace: exit 137 with `OOMKilled=false` distinguishes that forced termination from
normal completion. Its connection ended, no completed report was present, and
restart exposed `interrupted` without replay. Restarted services were stopped
again with a 5-second grace; baseline exit was 143 and candidate exits were zero.

Stop times include the Docker stop command and following inspect command,
including CLI overhead. They exclude later log reads and are neither engine
analysis time nor application-only shutdown latency. There is one completed
observation per service/work variant; these numbers are not a statistical
performance comparison. `shutdown_logged` checks the retained container logs,
which also preserve earlier messages across a restart.

The first probe run completed five baseline cases, then reused the old dynamic
port during the background restart readiness check and timed out. The service
remained running. That harness error was fixed by resolving the new port; all
four subsequent restart observations confirmed a changed port. The five valid
records were reused, and the unfinished cases resumed with identical frozen
binaries, finishing with exit zero. The original failure and raw records are
retained by SHA-256 in the JSON; no product source change or measurement rerun
was needed to obtain better numbers.

Each probe confirmed removal of its own container before deleting its fixture.
A separate final aggregate read initially failed because the local Docker
endpoint was unavailable after completion. After the saved local VM was
restored, a task-prefix query confirmed zero remaining probe containers. That
read failure is retained separately from the successful lifecycle observations.

The Linux [process regressions](../crates/printable-server/tests/shutdown_test.rs)
also cover SIGINT for all four entrypoints, accepted reconstruction drain, closed
new connections, completed CAD restart, and interrupted CAD/slicer restart after
normal or forced stops. Forced host-process tests explicitly kill only their
identified fake children; they do not establish Docker-wide cleanup. SIGINT,
active reconstruction, and slicer restart were not separate Docker trials here.

Validation completed with explicit zero exits for the focused shutdown test,
formatting, clippy with warnings denied, full Rust workspace suite, add-on tests,
repository-script tests, and documentation gate. Eight model-dependent Rust
tests and two published-model Python fixtures remained skipped; add-on tests had
no skips. The original documentation scan failed because mounted Cargo caches
exposed third-party Markdown. A source-only container with isolated Python
tooling passed without changing source or documentation rules.

The focused regression is reproducible with:

```sh
cargo test -p printable-server --test shutdown_test --all-features --locked -- --nocapture
```

For lifecycle reproduction, freeze the four service binaries, launch each with
`docker run --init` and the fixture controls above, wait for health, then use
`docker stop --time 5` while idle or after background admission. For an accepted
synchronous CAD request, use `--time 8` and release the fixture after the drain
log; leave it blocked under `--time 1` for forced termination. Inspect exit,
running/PID/OOM state, retained work files, and post-restart status. Resolve the
new published port after each restart. The raw JSON records the exact protocol
and candidate probe hash. The disposable probe runner was an ignored local
harness; the committed process regressions remain the runnable repository test.

Actual resource/backend failures, partial outputs after forced termination, and
unknown outcomes of already delivered external mutations remain possible. These
fixtures prove service signal handling and retained CAD recovery, not successful
completion of every backend operation. Follow the [operations guide](../docs/operations.md#stop-services)
for grace periods, checkpointing, and inspection before retrying work.
