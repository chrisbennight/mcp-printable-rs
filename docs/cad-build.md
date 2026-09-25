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
    "parameters": {"width": 80},
    "inputs": ["vendor/board.step"]
  }
}
```

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
A directory with neither terminal file indicates an interrupted or active build;
inspect it before choosing a new directory. Native diagnostics are retained in
`build-log.json` when the process completes. The build response contains compact
measurements and artifact references; the full component inventory stays in a
file. Retrieve or publish outputs with the existing `artifact` workflow.

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
