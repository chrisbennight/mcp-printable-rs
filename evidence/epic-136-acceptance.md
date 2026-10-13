# Epic 136 acceptance evidence

The goal of [epic 136](https://github.com/chrisbennight/mcp-printable-rs/issues/136)
is better performance, reliability, and agent efficiency while preserving native
modeling, inspection, assembly, retained jobs, immutable artifacts, and recovery.

This is **draft acceptance evidence**. Final source checks, matching-image
functional checks, and public release qualification remain pending. The
[JSON record](epic-136-acceptance.json) retains measurement conditions, outcomes,
source identities, limitations, and links to the child evidence.

## Admission and assembly

[Admission evidence](mcp-admission.md) measures a real server with blocked fake
Blender work. Candidate admission bounds retained calls while preserving controls.

| Offered calls | Baseline accepted/refused | Candidate accepted/refused | Baseline/candidate server peak KiB |
| ---: | ---: | ---: | ---: |
| 64 | 64/0 | 32/32 | 174,244 / 116,876 |
| 128 | 128/0 | 32/96 | 302,280 / 118,864 |

Separate legacy and modern wire trials retained accepted work after disconnect,
refused excess calls, drained each once, and admitted later work. Control groups
completed in 480.6/476.9 ms, within the declared two-second criterion.
HTTP 429 with `server_busy` refuses work before dispatch; accepted timeouts remain
uncertain and do not authorize blind mutation replay. Other finite request bounds
can still saturate; these observations are not a universal availability guarantee.

[Assembly evidence](assembly-input-memory.md) records server peak growth of
21.95/133.94 MiB on baseline versus 6.38/6.38 MiB on candidate for approximately
8/64 MiB per part. Four input copies became one immutable snapshot pair.
Both sources passed all 18 native volume, clearance, and interference trials.
Cancellation retains files and the execution lane until the worker actually exits.

Candidate native timings were slower: the largest separated median was
761.1 versus 407.9 ms in the worker, and 815.2 versus 438.1 ms for the whole call.
Sequential batches, different input filesystems, and uncontrolled load prevent
a causal speed claim. Peak memory is approximate kernel accounting, not an
allocation bound or machine-sizing recommendation.

## Actual full and selected agent trials

Agents used the same missing-file recovery task, closed 10 × 20 × 30 mm STL,
strict client, native oracle, and budgets of ten MCP requests and twenty local
commands. All completed trials recovered through the permitted fallback, matched
the 6,000 mm³ oracle, preserved the fixture, and downloaded 1,477 verified bytes.

| Source / discovery | MCP/local commands | Discovery decoded JSON bytes | Failed-schema responses / diagnostics |
| --- | ---: | ---: | ---: |
| Baseline / full | 5/6 | 209,836 | 1/3 |
| Baseline / selected | 7/8 | 12,601 | 1/3 |
| Candidate / full | 5/6 | 219,138 | 0/0 |
| Candidate / selected | 7/8 | 13,762 | 0/0 |

Candidate preserves actionable `not_found` and `isError` while making the required
missing-file result pass its discovered output contract. Both sources recover
successfully; completed trials made no incorrect calls or uncertain mutation replays.
Initialization is outside the MCP budget; local counts include the final report.

Selected discovery uses three requests instead of one and receives fewer bytes.
Decoded UTF-8 result bytes exclude HTTP/JSON-RPC framing and are not token counts.
Provider token usage, total model exposure, and exact inherited model identity
are unavailable. One completed trial per source/mode and parent-mediated wall
times do not establish population success rates or a causal latency improvement.

## Source and functional qualification

- Agent-trial baseline: `2e46398fb11bd99cc58dac256b82848e46b6eb0a`.
- Agent-trial candidate: `70414e47a9bbea9018888192665e03b2118cba6c`.

Admission, assembly, and shutdown retain the source identities in their child
records. Measurements used Linux/arm64 development binaries in Docker without
a GPU. Later optional head/bust changes were not exercised by the agent task;
historical identities and timings are not relabeled as final-release measurements.
Only completed agent trials are summarized. Setup and harness failures are
excluded from completed-trial counts. Some cleanup checks initially failed and
passed unchanged reruns; their causes remain unconfirmed.

Required source gates are cargo fmt, clippy with warnings denied, locked workspace
tests, addon and script unit tests, and docgate. Source regressions cover strict
errors, scene guards, unknown outcomes, one-use authority, admission, and supported
notification lifecycles. [Shutdown evidence](service-shutdown.md) covers drain and
restart without replay; a zero service exit does not prove job completion.

Existing `scripts/test-installation.py --recovery` and `scripts/smoke-release-pair`
exercise installed modeling/inspection, native assembly/motion, retained renders,
CAD/slicing, guarded attachment, verified delivery, restart, backup restore, and
damaged-metadata/disk-pressure recovery. Hosted matching-image qualification is
still required. Software and fake-backed lifecycle tests do not establish NVIDIA
performance, physical manufacturing fitness, or real printer stop behavior.

## Optional measurement runs

The repeated admission and native tessellation measurements are opt-in.
Functional saturation, native geometry, cancellation ownership, and shutdown
regressions remain in ordinary tests. On Linux with the contributor toolchain
and Python 3, run the optional measurements explicitly:

```sh
cargo test -p printable-server --all-features --locked --test admission_memory_test \
  increasing_concurrency_records_isolated_server_peak_memory -- --ignored --exact --nocapture
cargo test -p printable-server --all-features --locked --test assembly_analysis_test \
  increasing_tessellation_preserves_real_worker_measurements -- --ignored --exact --nocapture
```

These commands measure the checked-out source. The child reports describe the
historical experiments; reproducing their baseline comparisons requires those
recorded sources. Fresh results need their own source and environment description.

## Pending release acceptance

Follow the [release guidance](../docs/releases.md) to qualify a public image set
with its dependency inventories and attestations. Retain the actual source,
tested and published image identities, functional check results, and public
access result. Final source, image, and release qualification remain pending.
