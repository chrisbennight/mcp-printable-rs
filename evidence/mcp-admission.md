# MCP admission measurements

Ordinary tool work remains counted after stream headers or a client disconnect.
The candidate reserves separate capacity for observation, stop controls, and
notification streams. The frozen baseline released HTTP concurrency ownership
when response headers arrived and admitted more retained work.

Measurements were recorded on 12 October 2026 using disposable workspaces,
loopback HTTP clients, and a fake Blender bridge. The raw records, all sampled
memory values, source blob identifiers, and retained diagnostic hashes are in
[mcp-admission.json](mcp-admission.json).

## Source and environment

The baseline source revision is `2e46398fb11bd99cc58dac256b82848e46b6eb0a`. The candidate uses
`ac86e5f7bd666577723f1ae69edde3c6b19ba89d` with the admission changes identified by Git blob hashes in the raw
record. The frozen baseline memory build overlays only the new integration test;
its production source files match the frozen revision.

Both memory runs used Rust 1.97.1 on Linux/aarch64, debug binaries, eight available
logical CPUs, and a Linux VM reporting 12,220,972 KiB of total memory. Docker had
no separate container memory or CPU limit. The toolchain image's immutable ID is
recorded with the resource details. These are local software measurements; they
do not qualify the supported Linux/amd64 NVIDIA installation.

## Server-process memory

Each concurrency level starts a fresh real `printable-server` subprocess and
initializes a legacy MCP session using protocol 2025-11-25. Each offered
`blender_execute` call carries 589,839 UTF-8 bytes of source and a one-second
requested execution budget. The fake bridge delays its first delivered command
for thirty seconds. The test observes retained work before that delay expires.

Memory comes from that server's `/proc/PID/status` `VmHWM`; clients and the fake
bridge run outside the measured process. The table reports the greatest of the
readings before the burst, while work is blocked, and after health and status.
The raw record retains the individual readings and growth from the first
reading. Kernel RSS accounting and debug-build overhead remain part of this
measurement.

| Offered calls | Baseline accepted / refused | Candidate accepted / refused | Baseline peak KiB | Candidate peak KiB | Health + status ms, baseline / candidate |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 1 / 0 | 1 / 0 | 38,672 | 37,960 | 47.7 / 45.0 |
| 16 | 16 / 0 | 16 / 0 | 73,352 | 72,088 | 42.9 / 51.7 |
| 64 | 64 / 0 | 32 / 32 | 174,244 | 116,876 | 49.6 / 43.4 |
| 128 | 128 / 0 | 32 / 96 | 302,280 | 118,864 | 43.2 / 50.9 |

Each row is one observation, not a statistical latency comparison. The baseline
accepted every offered call; the candidate refused excess admission. Every row
had one delivered backend command and no completed workload calls while sampled.
The measurement does not establish an allocation bound, throughput guarantee,
or peak memory for other workloads. The fixture deliberately ends with SIGKILL
and observes that exit; this establishes fixture disposal, not graceful drain.
Initial baseline samples are also retained in the raw record. One earlier kernel
high-water reading fell slightly between observations, so the final test keeps
every reading and reports the greatest observed value.

## Stream ownership and controls

The wire regression offers forty calls across two legacy sessions or the modern
stateless lifecycle. It blocks the fake bridge until after the original clients
disconnect, probes controls, then releases and drains the accepted work.

| Observation | Baseline legacy | Candidate legacy | Candidate modern |
| --- | ---: | ---: | ---: |
| Protocol | 2025-11-25 | 2025-11-25 | 2026-07-28 |
| Offered calls | 40 | 40 | 40 |
| HTTP 200 headers received while blocked | 40 | 32 | 0 |
| Requests waiting for headers | 0 | 0 | 32 |
| HTTP 429 refusals | 0 | 8 | 8 |
| Time to observe offers and refusals | 891.8 ms | 836.1 ms | 832.4 ms |
| Further calls refused after original clients close | Not reached | 8 | 8 |
| Health, observation, and stop-control probes | Not reached | 480.6 ms | 476.9 ms |

The baseline assertion failed as expected because all forty calls remained
accepted. Both candidate lifecycles passed: accepted work retained capacity after
disconnect, drained once released, delivered each accepted command once, and
allowed a subsequent successful call. The separate admission unit tests passed
for control classification, subscription capacity, and immediate refusal after
server cancellation.

Modern requests carry per-request protocol and client metadata,
`MCP-Protocol-Version`, `Mcp-Method`, and `Mcp-Name` for `tools/call`. The transport
can wait for its first protocol message before sending modern response headers.
An earlier helper omitted `Mcp-Name` and was rejected before tool dispatch; those
failed diagnostics are retained and are excluded from the final observations.

## Admission and retry contract

The candidate has 32 body-reader slots, 32 ordinary tool-work slots, eight control
slots, and sixteen notification-stream slots. The existing per-request body
limit is 1,463,640 bytes. These are transport boundaries, separate from model
complexity, generated artifact size, and caller-selected work budgets. They do
not form a complete process memory limit or a machine-sizing recommendation.

The control classification includes `status`; render-job `get`, `list`, and
`cancel`; CAD and slicer `status` and `cancel`; print `status`, `cancel`, `pause`,
and `stop`; printer `status`; and artifact `transfer_status`. Other tools use
ordinary work capacity. Health routes do not consume MCP admission slots.
The wire probes used missing handles and unconfigured printer/native services;
they establish reachable transport and dispatch, without claiming a real
printer stop or native-job cancellation.

Admission refusal returns HTTP 429 with JSON `error: "server_busy"` before tool
dispatch. It establishes that this request did not start work through the MCP
handler. A disconnect or timeout after acceptance does not establish that a
mutation failed. Inspect scene or retained job state before deciding whether to
retry an uncertain mutation. No automatic retry delay is supplied.

Reserved controls address saturation by retained tool work. Body readers remain
shared: incomplete uploads can exhaust them before classification. Control and
subscription capacities are also finite. The measurements do not establish
availability under every traffic pattern.

After acquiring a slot, the candidate checks the cancellation token and refuses
new admission when cancellation is observed. Its watcher closes the admission
semaphores. Existing work or streams keep their owners until completion or
closure. Runtime shutdown and backend recovery have separate contracts; these
measurements do not prove drain of every accepted effect on process termination.

## Reproduce

Run on Linux from a checkout containing the admission tests:

```sh
cargo test -p printable-server --test admission_memory_test --locked -- --nocapture
cargo test -p printable-server --test mcp_endpoint_test \
  retained_tool_work_stays_bounded_after_stream_headers_and_disconnects \
  --locked -- --exact --nocapture
cargo test -p printable-server --lib admission::tests --locked
```

The final candidate commands above exited successfully. To reproduce baseline
memory, overlay only `admission_memory_test.rs` on the frozen revision and run
the first command. The test records every level without asserting the candidate
capacity against the baseline. When sharing a target directory between these
source trees, clean `printable-server`, `printable-geom`, and `printable-flame`
before switching; a stale geometry artifact caused an earlier baseline build
failure, which is retained with the diagnostics.

Provider-token use, response-byte volume, actual-agent completion rates, GPU
performance, and native-engine outcomes were not measured. Argument bytes above
describe source input and are reported separately from those unavailable metrics.
