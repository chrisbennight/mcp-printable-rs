# Project CAD builds

`cad_build` runs general CadQuery scripts and imports STEP assemblies without
changing Blender's live scene. Create a project with `project`, then write or
ingest its source and input artifacts. Paths in build requests are relative to
that project's root.

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

Native execution uses the request's `timeout_seconds` (default 600, maximum
1800). Admission has no queue: an occupied worker rejects the build without
creating its output directory. The retained `phase` distinguishes input
snapshotting from native execution, with admission and native-start timestamps.
Input snapshotting is bounded by the input count and combined byte limit; the
execution deadline starts when the native command runs. Numeric native progress
is unavailable and remains null.

Status is `admitted`, `running`, `completed`, `failed`, `cancelled`, or
`interrupted`. Cancellation during input copying is honored before native
execution; during native execution it kills the owned process group. A completed
artifact commit wins a concurrent cancellation request. Published artifacts and
inputs are retained in either outcome. Cancellation of a terminal build returns
its existing terminal state. It does not remove files or submit new work.

The default `background: false` still waits for the original synchronous result,
but disconnecting that waiter leaves admitted work running. The direct client's
individual HTTP timeout is shorter than a legitimate long build, so prefer
background admission and separate status calls. Use the original handle after
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
STL uses absolute linear tolerance. Builds accept a deadline up to 1800 seconds,
defaulting to 600, and a combined input budget of 1 GiB. The worker accepts one
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
