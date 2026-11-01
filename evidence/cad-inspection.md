# CAD inspection workflow evidence

Focused inspection avoids unrequested deliveries and geometry checks. Retained
native models support later questions and outputs without repeating STEP
translation or Python execution. Existing full builds remain available.

## Measured boundary

[Native measurements](cad-inspection.json) use the public fixture in
[cad/benchmark.py](../cad/benchmark.py): a generated board with placed repeated
connectors and discrete components, and its STEP exchange assembly declaring
inches. This is a synthetic vendor-style assembly, not customer data or a
performance claim about large vendor files. Source import still loads the
complete assembly.

The recorded run used Darwin/arm64, Python 3.14.9, CadQuery 2.8.0 and OCP 7.9.3.1.
Times are one ordered sample in a warm Python process. They exclude worker
startup, MCP transport, source snapshots and downloads. Cache effects are not
controlled. The supported Linux/amd64 installation is verified separately by
container CI; these timings do not predict its performance.

| Native operation | Generated source | STEP source |
| --- | ---: | ---: |
| Full build, all delivery formats and normal checks | 0.133 s | 0.142 s |
| First focused inspection, retaining native geometry | 0.019 s | 0.024 s |
| First focused inspection, no retention | 0.016 s | 0.022 s |
| Reused features and explicit validity question | 0.009 s | 0.008 s |
| Reused selected STEP output | 0.004 s | 0.004 s |

Retention stored 129,675 bytes for the generated fixture and 101,105 bytes for
the imported fixture. Initial output files totalled 139,351 and 113,321 bytes,
compared with 602,577 and 627,939 bytes for the corresponding full builds.
These totals exclude the server's immutable input copies. Each follow-up also
retains its native-model input snapshot under the existing workspace policy;
reuse saves translation work, not all storage or I/O. The worker still reopens
native geometry per request rather than retaining an unbounded live interpreter.
Peak memory was not measured.

The inspect/follow-up/export sequence uses three logical CAD work requests and
no task-specific measurement script. Questions and selected outputs can also
share a single request when the target is already known. Both old and new
interfaces can answer basic bounds in one request; the improvement comes from
avoiding unrelated work and providing reusable native questions, not a universal
claim about fewer calls. The original full-build interface needed caller-written
Python for these richer questions.

## Discovery and result size

[Contract measurements](cad-inspection-contracts.json) serialize the public
catalog with default optional-worker availability and compare it with repository
revision `cc5f643bc48e44582afd11107a3d8d3fd6df82d4`. They count compact UTF-8 JSON,
excluding transport wrappers and server instructions.

| Payload | Before | With native questions and requested outputs |
| --- | ---: | ---: |
| Full tool catalog | 219,703 bytes | 234,527 bytes |
| CAD tool with all its contracts | 12,832 bytes | 27,656 bytes |
| Focused inspect contract | Unavailable | 16,166 bytes |
| Contract index | 2,418 bytes | 2,437 bytes |

The expanded full catalog costs more. Clients supporting focused discovery can
load the index and selected CAD contract instead of unrelated contracts. Hosts
that always load the full catalog receive that additional cost; this work does
not claim their catalog shrank. Bounds and feature pages keep complete
inventories in artifacts, including the full selected export scope. The recorded
first-answer native reports were 616 and 785 JSON bytes. Richer requested answers
are larger. Native report sizes exclude server result and artifact descriptors.

Actual model token usage is unavailable: these scripted measurements invoke no
language model and receive no provider usage. Bytes are not token counts, and
neither these timings nor these byte counts establish model-token savings.

## Capability verification

The native smoke verifies rotated and repeated occurrences, local and assembly
frames, region selection, pagination, missing and ambiguous matches, native
round trips and face colors, batched geometric questions, selected outputs and
partial success. The follow-up test changes the original script and makes any
attempt to execute Python or import STEP fail, while reused geometry still
answers questions. A deliberately failed GLB output leaves measured evidence
and other outputs available.

Focused Rust lifecycle checks verify digest mismatch before native execution,
typed public results, retained failure evidence and caller-selected work
budgets. The existing installation smoke additionally exercises generated and
STEP inspection, model references, batched partial answers and a verified
selected STL download through the real public MCP interface. Its CI artifact
`cad-inspection-evidence` records observed first-answer and follow-up times and
public result sizes, including worker/transport costs. It reports unavailable
model usage explicitly. The same structured arguments and returned identities
can be composed by gateway Code Mode; no gateway-only endpoint or byte relay is
required. No deployed gateway or model-host usage measurement is claimed here.

## Reproduce

Use the pinned native CAD dependencies from `cad/requirements.txt`, then run:

```sh
python cad/smoke.py
python cad/benchmark.py /tmp/cad-inspection-new.json
cargo run -p printable-server --example cad-contract-metrics --all-features --locked
```

The benchmark destination must be new. Use Linux for the Rust command. For the
historical contract comparison, copy the same example into a clean checkout of
the baseline revision and rebuild the server/workspace crates; do not reuse stale
compiled package artifacts between source candidates. The example reports
missing selected contracts as null on older sources.

Normal container CI runs the public MCP workflow on isolated fixtures, without
a GPU, printer or production service. See the [CAD guide](../docs/cad-build.md)
for selection, reuse, output settings and reconciliation after a lost response.
