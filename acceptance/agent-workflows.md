# Agent workflow evaluation

This is the acceptance plan for the [Printable product refactor](../docs/product-refactor.md).
It complements the [existing product corpus](README.md). It does not report
unexecuted tasks as passes or infer model tokens from JSON or PNG byte counts.

## Baseline identity and existing evidence

The baseline below is historical. Its unavailable features describe that
earlier revision, not the current product. Site-specific release evidence is
not retained here and is not required to run the current acceptance corpus.

The starting source revision is
`c4c0f7c1b3175b71b0bf2921f909fba113461c07`, freshly fetched from `origin/main`.
Its historical release catalog advertised forty prefixed tools; the
[current catalog](../smoke/expected-tools.txt) records the new release surface. The approved target and exhaustive mapping are in the product plan.

The baseline has a persistent background Blender process. Its main-thread pump
serializes edits and synchronous observations; durable render jobs temporarily
load checkpoints and retain the Blender lane while restoring the live session
around the work. It has no native UI viewport or editor capture contract.

Existing deterministic evidence is encoded by the external
[`printable-smoke`](../crates/printable-server/src/bin/printable-smoke.rs) driver,
the [paired-container smoke](../scripts/smoke-release-pair),
[Blender integration smoke](../addon/integration_smoke.py), and
[production GPU smoke](../scripts/smoke-blender-gpu). They exercise real public
tools, decoded artifacts, manufactured products, and certified motion. Pure
tests use fakes and never substitute for these deployed-runtime boundaries.

Historical delivery included modeling guide execution, selective inspection,
rendering, discovery, and restoration of the pre-test scene. The site-specific
records are not public qualification evidence. Re-run those workflows against
the intended image set; do not infer controlled efficiency measurements from
the historical functional observations.

| Baseline measurement | Recorded status |
| --- | --- |
| Existing product/wire regression suite | Available in source; run against the candidate image set |
| Live selective-modeling workflow | Historical report only; current qualification required |
| Native viewport/editor capture | Unavailable in this baseline |
| Live edits during independent worker rendering | Unavailable in this baseline |
| Model input/output and image token comparison | Not measured |
| Repeated held-out agent task success | Not measured |
| Comparative feedback latency and peak RAM/VRAM | Not measured |

## Evaluation tasks

Each trial starts from an immutable fixture and a written user outcome. A test
must name its geometric or visible oracle before an agent acts. Keep setup
separate from the agent's available instructions, and use only public product
capabilities during the task. Rendered appearance alone is never a solid or
clearance certificate.

| Task | User outcome | Required evidence |
| --- | --- | --- |
| Enclosure revision | Change the enclosure dimensions while retaining usable bosses and ribs | Requested dimensions, valid solid, specified profile, useful exterior and interior views |
| Occluded feature | Locate and correct a defect behind another part or inside a cavity | Agent obtains a revealing view; exact targeted correction; unrelated geometry preserved |
| Surface defect | Distinguish reversed normals from shading or material appearance | Correct diagnosis and repair, checked topology/normals, repeat view under fixed display settings |
| Modifier comparison | Explain and revise the difference between base and evaluated geometry | Authored modifier state plus base/evaluated observations with comparable framing |
| Nodes and materials | Revise a procedural material or Geometry Nodes result | Authored node/link evidence and corresponding changed appearance without unrelated edits |
| Mesh selection | Inspect a specified edge/face selection and perform the requested edit | Native visible selection, bounded matching structured state, and correct resulting geometry |
| UV inspection | Find and correct a specified UV-layout problem | Relevant UV editor observation and data; material result where applicable |
| OpenSCAD parameters | Revise a bracket or grip through typed parameters | Correct profile/variant application, valid STL, decoded PNG/SVG, honest wall/clearance scope |
| Mechanical video | Deliver a complete articulated sequence with the required clearance | Exact checkpoint input, complete-arc certificate before frame one, decoded video and timing |
| Concurrent production | Continue inspecting and revising live work while a checkpoint render runs | Successful live operation, unchanged job input, no worker changes copied into the live scene |
| Recovery | Recover from a runtime/display/worker failure | Explicit state transition, no stale successful observation, known checkpoint recovery, no unsafe mutation retry |

For held-out trials vary dimensions, object names, defect location, viewing angle,
and material/node structure without changing the intended contract. Do not give
the agent the oracle or tune the interface against only the development fixtures.

## Comparison conditions

Compare the existing observation surface, enhanced controlled headless views,
and native viewport/editor feedback using equivalent tasks and budgets. Where
a condition cannot express the task, record that capability gap explicitly
rather than forcing a fabricated baseline or awarding success for an unrelated
render. Keep model identity and agent instructions fixed within a comparison;
record other runtime differences that cannot be held constant.

Use repeated trials. Preserve trial order and failures, and report distributions
and task-level results rather than only the best run. Do not choose a success
threshold after seeing a result. Correctness and existing product preservation
are release gates; token and latency comparisons inform proportionate fixes and
must not erase required capability merely to improve a score.

## Observation and execution evidence

The [direct-client discovery fixture](../docs/selective-contracts.md) records
full versus selected contracts for a small OpenSCAD build and dimensional
check. The installation test runs it against isolated containers and can
retain its evidence files. It does not run an agent or establish provider
token savings; those fields remain explicitly unavailable.

Each retained trial record identifies:

- source release, deployed image identities, model identity, prompt/fixture
  references, trial condition, caller budgets, and start checkpoint;
- scene generation/model revision, frame, view configuration, capture method,
  and convergence state when applicable;
- ordered public calls and bounded results with credentials excluded, plus
  references to full artifacts stored outside the model's context;
- oracle results, actual edits, invalid-call/recovery outcomes, elapsed time,
  and resource observations with sampling method;
- actual model text/image usage when the execution harness supplies it;
  otherwise an explicit unavailable field, never a byte-based token estimate.

Do not claim a freshly requested redraw is a completed capture. Record how
dependency evaluation and drawing freshness were established. Keep model
revision distinct from inspection-view changes so a camera orbit does not imply
an edited part. Comparisons should reuse a view definition unless reframing is
part of the task.

The evaluation runner must not log bearer values, provider credentials, raw
environment maps, or unrelated user artifacts. Keep tests isolated from user
state. Any authorized live verification checkpoints the current scene before
mutation, restores it afterward, and verifies restoration. A failed restore is
an actionable failure, not cleanup noise.

## Migration and delivery checks

The current repository consumer surface includes tool dispatch/catalog tests,
the public smoke driver, release catalog, modeling/design/render resources,
README and protocol documentation, and deployment/gateway smoke configuration.
The production gateway also uses an explicit manifest whose catalog update
requires its own governed approval. Discover its current contract and active
callers at cutover; this list is not proof that no external consumer exists.

During transition, legacy entry points may call the same implementation. At
final cutover the advertised catalog is exactly the target in the product plan.
Durable job records and user artifacts must remain readable across the upgrade
even when old tool names are no longer advertised.

Final evidence must cover direct MCP calls and gateway Code Mode discovery,
typed result consumption, bounded visual observations, resource discovery,
artifact publication, asynchronous job completion/cancellation, and recovery.
All required review/CI gates and the exact-image production GPU checks remain
mandatory. The epic is complete only after the integrated workflow works live.
