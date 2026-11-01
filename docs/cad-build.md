# Project CAD builds

`cad_build` runs general CadQuery scripts and imports STEP assemblies without
changing Blender's live scene. Create a project with `project`, then write or
ingest its source and input artifacts. Paths in build requests are relative to
that project's root.

## Focused component inspection

Use `cad_build` action `inspect` to find components and measure selected native
CAD bounds without creating STEP, STL, or GLB exports, validating every solid,
or searching every face for holes. It accepts STEP sources and Python scripts
assigning `result`, with the same retained input and project conventions as a
build. It leaves Blender unchanged.

```json
{
  "action": "inspect",
  "params": {
    "project_id": "enclosure",
    "source": "vendor/board.step",
    "output_dir": "inspection/connectors",
    "selection": {"name_contains": "USB"},
    "frame": "assembly",
    "limit": 10
  }
}
```

Inspection defaults to background admission. Use `status` or `cancel` with the
returned build handle, as below; `background: false` waits synchronously.
The positive `timeout_seconds` budget defaults to 600 seconds. The initial
source import still loads the assembly; selecting a component does not promise
partial STEP translation. Follow-up requests can reuse the returned native model.

The completed result has `scope: "cad_inspection"` and a typed `inspection`
answer. It includes matching component occurrence paths, original names,
placements, and bounds in millimetres. `frame: "assembly"` measures the placed
component along the assembly axes; `frame: "component"` measures its own local
axes. The placement always maps local coordinates to assembly coordinates.
Bounds use native geometry without creating triangulation; their numerical
precision follows the native geometry kernel, not a manufacturing tolerance.

Select exact occurrence paths with `selection.component_ids`. Combine selectors
to narrow a match. `selection.region_mm` contains `min` and `max` XYZ arrays and
matches component bounds intersecting that region in assembly coordinates,
regardless of the answer's frame. A bounds intersection is a search filter,
not proof that the shapes touch. Missing requested identifiers are explicit;
name searches return all matches rather than guessing one intended component.

`offset` and the positive `limit` page the answer; the default page size is 20.
`matched_count` and `next_offset` expose remaining matches. The retained
`components.json` contains the complete names, hierarchy, and placement inventory
without computing every component's bounds. Only the selected answer page is
measured unless the requested spatial search needs bounds for filtering.
No delivery or manufacturing qualification is inferred from inspection.
Read `printable://contracts/cad_build/inspect` for the focused input and output
contracts. Existing `model` and `import_step` actions retain full-build behavior.

## Reuse, questions, and selected outputs

Inspection retains native geometry by default and returns `model` with a
project-relative `path` and SHA-256. Supply that entire object as `model` in a
later `inspect` or `export` request instead of `source`. Reopening it does not
run Python or read STEP again. Changing the source filename cannot change the
retained geometry; changing the retained bytes causes a digest error. A new
source or parameter revision needs a new inspection. Set `retain_model: false`
when no follow-up is needed; existing retained models are not duplicated as new
model outputs during reuse. Each request still retains its own immutable input
snapshot, including the referenced native model, under the existing workspace
budget. Account for those additional stored bytes when making many requests.

Native retention preserves assembly hierarchy, occurrence paths, placements,
names, colors and exact topology without triangulation. It uses workspace
storage and requires the same native worker versions. Missing or incompatible
state fails explicitly; inspect completed evidence or deliberately import the
source with the current worker. No script is replayed automatically. Retention
is a geometry snapshot, not editable parametric history or a promise of face
identities across revisions. Original input hashes, parameters and snapshots
remain in the initial inspection directory.

Use `questions` to combine independent geometric questions in one loaded model:

| Kind | Required scope | Evidence |
| --- | --- | --- |
| `features` | One occurrence; optional `offset`/`limit` | Native planar and cylindrical faces with face indices, origins, directions, areas and cylinder radii |
| `distance` | `first`/`second` targets; optional `face_index` on each | Minimum native separation in mm |
| `alignment` | Two explicit planar or cylindrical face targets | Unoriented angle between plane normals or cylinder axes |
| `section` | One occurrence, `origin_mm`, nonzero `normal` | Native plane intersection edge count, length and bounds |
| `validity` | One occurrence | Explicit native geometry validity and solid count |

Every question has a unique caller-selected `id`. A target can use an exact
`component_id` or `selection` with the same name, identity and region selectors
as the outer request. Omitting both uses the outer selection. Measurement
targets must resolve to exactly one occurrence; absent or ambiguous matches
are explicit unavailable answers. A unique name can therefore select and
measure a known part in the first request, without a discovery call.
Features, sections and
validity use the requested frame; pair measurements use assembly coordinates
and say so in their answer. Face indices apply to that component in that model.
Each answer states its method and frame; units are mm, mm² and degrees as named
in evidence fields. Native kernel precision applies, not an asserted physical
tolerance. A cylinder does not establish a thread or fastener fit; zero distance
does not distinguish contact from overlap, and alignment alone does not establish
coaxiality. Unsupported or absent geometry yields `status: "unavailable"` for
that question while other answers remain accessible. No validity check runs
unless requested. Known component identities can be measured directly without
an inventory round trip; question targets are independent of the bounds-page
selection.

Choose `exports: ["step", "stl", "glb", "svg"]` with only the formats needed.
Both `inspect` and `export` accept the same source/model, selection and question
parameters; `export` requires at least one format. Multiple outputs share one
loaded model. Exports cover all matching occurrences, independently of bounds
pagination. An empty selection means the complete assembly. Missing explicit
identifiers fail the output rather than silently reducing its scope. Assembly
coordinates preserve placements and hierarchy. Component-coordinate export
requires one occurrence and preserves its local geometry.
Each output reports its selected count and a JSON pointer to the complete
selected identifiers in the retained `components.json` artifact, keeping large
assembly scope out of the inline answer.

STEP is native exchange geometry, STL and GLB are tessellated deliveries, and
SVG is an optional projected preview. Mesh tolerances are explicit in each
output result, with the full-build defaults described below. SVG uses the
native exporter's default isometric projection and display sizing; measure
native geometry rather than SVG pixels. GLB follows glTF's metre convention;
geometry evidence and STL/STEP use mm. Existing artifact downloads, Blender CAD
attachment and slicer inputs consume these normal project files.

For example, after an initial result, a normal direct MCP client can compose a
follow-up without relaying model bytes:

```python
target = first["inspection"]["components"][0]["id"]
followup = client.call("cad_build", {"action": "export", "params": {
    "project_id": "enclosure", "model": first["model"],
    "output_dir": "inspection/connector-details", "background": False,
    "selection": {"component_ids": [target]}, "exports": ["step"],
    "questions": [
        {"id": "faces", "kind": "features", "component_id": target},
        {"id": "valid", "kind": "validity", "component_id": target}
    ]
}})
```

The result's `completion` is `partial` when a question is unavailable, a
requested output fails, or artifact publication is unconfirmed. Check every answer and output status; a terminal job
can complete with partial useful evidence. Detailed inventory and diagnostics
remain artifacts. Successful deliveries are retained alongside failures, and
completed measurement evidence is saved before optional exports. Full model
delivery and manufacturing qualification remain separate from these questions.
Verified answers are committed to `measurements.json` before other artifacts
are published. `publication_errors` identifies unconfirmed artifact writes,
and affected outputs have `status: "publication_unconfirmed"`. Inspect the
output directory before retrying: a write error does not prove a file is absent.
A rejected large output does not prevent a smaller requested output from being
published. Missing model or inventory publication is also explicit in this list;
it does not discard the saved geometric answers. The retained evidence excludes
delivery qualification.
See [measured workflow evidence](../evidence/cad-inspection.md) for costs and limits.

## Full CAD builds

Optionally capture a [design revision](project-revisions.md) and pass its
identity as `revision`. The build then uses verified retained source bytes and
the revision's parameter values, returning requirement checks and a retained
measurement identity alongside its normal artifacts.

```json
{
  "action": "model",
  "params": {
    "project_id": "enclosure",
    "source": "housing.py",
    "output_dir": "builds/revision-1",
    "background": true,
    "parameters": {"width": 80},
    "inputs": ["vendor/board.step"],
    "qualification": {
      "policy": "printable_part",
      "units": "mm",
      "dimensions_mm": [80, 40, 20],
      "dimension_tolerance_mm": 0.05,
      "solid_count": 1
    }
  }
}
```

`background: true` returns retained admission state and a `build` handle with
`project_id` and `output_dir`. Query it with `cad_build` action `status`; a
completed state includes the normal result in `result`. Use action `cancel`
with the same handle to request cancellation. For example:

```json
{"action":"status","params":{"project_id":"enclosure","output_dir":"builds/revision-1"}}
```

Native execution uses the request's positive `timeout_seconds` work budget
(default 600; no configured maximum). Admission has no queue: an occupied worker
rejects the build without
creating its output directory. The retained `phase` distinguishes input
snapshotting from native execution, with admission and native-start timestamps.
Input snapshots stream observed source bytes under the configured workspace
budget; the
execution deadline starts when the native command runs. Numeric native progress
is unavailable and remains null.

Status is `admitted`, `running`, `completed`, `failed`, `cancelled`, or
`interrupted`. Cancellation during input copying is honored before native
execution; during native execution it kills the owned process group. A completed
artifact commit wins a concurrent cancellation request. Published artifacts and
inputs are retained in either outcome. Cancellation of a terminal build returns
its existing terminal state. It does not remove files or submit new work.

The default `background: false` still waits for the original synchronous result,
but disconnecting that waiter leaves admitted work running. The caller's MCP
client owns its end-to-end request deadline; worker forwarding imposes no
independent fixed HTTP timeout. A client deadline can expire while a legitimate
build continues, so prefer background admission and separate status calls.
Use the original handle after
any lost response; never automatically repeat a model/import request. If no
active worker owns retained unfinished state after restart or interrupted
completion recording, status reports `interrupted`, even if some output files
exist. Inspect them before choosing a new output directory. Older completed
builds with only `report.json` remain readable as completed legacy reports;
missing or unreadable metadata never becomes invented active progress.

OpenSCAD waits for its concurrency permit before input preparation; that wait
is outside its native subprocess budget. Geometry analysis also waits for its
shared execution lane. Neither gains a retained CAD handle from this interface.
A transport timeout does not establish whether native work started or finished.
See the [architecture guide](architecture.md).

The Python script receives `cq` and a `parameters` dictionary and assigns a
CadQuery Workplane, Shape, or Assembly to `result`. Imports and arbitrary Python
remain available inside the dedicated CAD container. The working directory
contains private input copies with their project-relative paths. This is
native code execution, not a Python language sandbox.

For assembly conversion, select `import_step` and supply a `.step` or `.stp`
source. The native reader resolves the source's units to millimetres and retains
component placements and colors. The source remains available alongside the
derived STEP, STL, GLB, component inventory, and numerical report. Meshes and
images do not establish exact manufacturing fit.

Each build requires a new `output_dir`. It contains `request.json`, input
snapshots with SHA-256 provenance, and eventually `report.json` or
`failure.json`. `report.json` is written after all output artifacts are committed.
Query retained status to distinguish active work from interruption before
choosing a new directory. Native diagnostics are retained in
`build-log.json` when the process completes. The build response contains compact
measurements and artifact references; the full component inventory stays in a
file. Retrieve or publish outputs with the existing `artifact` workflow.

Status also reports the last observed `native_stage`: source reading, component
inspection, STEP export, STL export, GLB export, geometry checks, or native
completion. This describes observed work, not a percentage or an estimated
finish time. `progress` remains null.
Unavailable or malformed stage metadata is reported as `progress_error`; it
does not turn a successful native operation into a failure or hide its logs.

Timeout and cancellation retain the bounded stdout/stderr captured before the
native process group was stopped. Terminal status lists committed diagnostic
and partial output references in `available_artifacts`. The worker retains only
files the native adapter marked complete, so a partly written mesh is not
presented as a finished export. Basic dimensions are saved before exports in
`measurements.json`, with `scope: "cad_measurements"` and `completion: "partial"`.
That evidence does not imply completed delivery or manufacturing qualification.
The normal full report still requires the complete build to succeed.

Use the direct client's runtime wait when an admitted job needs time:

```sh
python3 scripts/printable_client.py wait-cad enclosure builds/revision-1 --timeout 900
```

It queries the retained handle and returns terminal state, including failures
and available evidence, without resubmitting work. The waiting budget does not
change the native budget or cancel a build. Reconcile the same handle after a
lost connection or expired wait. A worker restart still reports unfinished work
as interrupted; files not committed to the workspace are not promised to survive
that restart, and no automatic native resume or script replay is attempted.

## Completion and delivery qualification

A response with `completion: "completed"` means the native exports and retained
report were published. Check `qualification` separately before accepting the
deliverable. Its scope is `cad_delivery`, with `status` equal to `passed`,
`failed`, or `incomplete`. A failed or incomplete qualification still returns
the exported artifacts, source snapshots, hashes, and diagnostics for inspection.
An execution or publication error remains a tool error with retained failure
evidence when available.

`qualification.policy` defaults to `inspection` for compatibility with general
modeling and imperfect imports. It requires the requested delivery units and
any explicitly supplied dimension or solid-count requirements. Geometry
validity and solid-volume diagnostics remain visible but do not prevent an
inspection result from passing its policy. A surface can therefore pass an
inspection policy without being accepted as a printable solid.

Select `printable_part` to additionally require valid geometry, positive-volume
solids, and target dimensions. Missing measurements or omitted target dimensions
produce `incomplete`, never an assumed pass. Multiple solids are allowed; supply
`solid_count` when their exact count matters. Dimensions are the assembly's
axis-aligned X/Y/Z bounds in millimetres. Tolerance is an absolute nonnegative
distance applied to each axis. Current exports use `mm`; requesting delivery
in `inch` produces a units failure without silently rescaling the model.
STEP source units are independently converted to millimetres by the importer.

Each criterion identifies its status, whether the selected policy requires it,
its report-relative evidence pointer when measured, and a next action when
needed. `passed` establishes only the required checks of that policy. Physical
performance remains `physical_test_required`; wall suitability, fit, strength,
material, supports, and print-process qualification require separate evidence.
The optional source `parameters` dictionary does not by itself establish any
dimensional invariant.

Existing requests without `qualification` retain inspection/export behavior.
Existing reports without the new fields carry no delivery-policy evidence;
rebuild from their retained source when that evidence is required. Recommended
clients check completion and qualification separately, then inspect all
remaining physical and manufacturing requirements before fabrication.

Linear meshing tolerance defaults to 0.05 mm and angular tolerance to 0.1 radians.
STL uses absolute linear tolerance. Builds accept a positive caller-selected
deadline, defaulting to 600 seconds. Input counts, source sizes, and generated
artifacts have no product capacity ceiling. The worker accepts one
active build; busy responses do not enqueue hidden work. A timeout kills the
native process group. Render jobs retain their separate lifecycle.

## Worker deployment

Build the Dockerfile's `cad-runtime` target from the same source revision as the
server. The standalone Compose configuration supplies `PRINTABLE_CAD_ENDPOINT`, the
shared workspace mount, memory/CPU/PID limits, a read-only root filesystem, and
private networking. The CAD worker receives no gateway, printer, or secret-store
credentials and has no Docker socket. Its HTTP listener is a private backend
interface and must not be published. General modeling remains available when
printer or slicer services are unavailable.

CadQuery is provided by the [official package](https://pypi.org/project/cadquery/)
and uses the [upstream import/export API](https://cadquery.readthedocs.io/en/latest/importexport.html).
The native adapter lives in [cad/build.py](../cad/build.py); Rust owns request
validation, bounded admission, process lifetime, and artifact publication.
