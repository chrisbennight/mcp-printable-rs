# Native slicing

For a worked model-to-package example followed by optional printer delivery,
see [Slice, review, and send a print](workflows/model-to-printer.md).

The `slice` workflow uses a separate, credential-free OrcaSlicer worker. It shares
the project workspace with the server and other engines, but cannot control a
printer. The packaged worker uses OrcaSlicer 2.4.2 and its BBL profile collection.
Profile availability is not a claim of compatibility with every printer.

## Why Printable maintains an OrcaSlicer fork

The packaged [OrcaSlicer fork release](https://github.com/chrisbennight/OrcaSlicer/releases/tag/local-2.4.2-63c97a1)
adds `--capture-workspace`, the native engine action behind
[`slice.capture`](#native-layout-capture). It opens a project or prepared
3MF under an isolated display and renders the requested Orca workspace view.
The caller can select plates and objects, control the camera and replay the
resolved view, and show or hide beds, plate identifiers, and Orca's estimated
prime towers. Printable binds the image and resolved view to the exact source
artifact and retains them as observation evidence.

[OrcaSlicer 2.5.1](https://github.com/OrcaSlicer/OrcaSlicer/releases/tag/v2.5.1)
can generate plate thumbnails through its CLI. Workspace capture still needs
the fork for the selection, camera, and native metadata this workflow requires. The current fork also backports CLI thumbnail,
per-filament and tool-change bounds, and STL reader fixes to its 2.4.2 base.
Equivalent fixes are present upstream in 2.5.1. The thumbnail path and those
backports can use the upstream implementations when changing the base version.

When changing the packaged engine, update the worker's version pin and the
readiness assertion in [the native slicing smoke suite](../slicer/smoke.py)
together. Run the Verify native slicing CI step against the candidate image.
It checks capture readiness, multi-plate selection, camera replay, prepared 3MF
identity, source immutability, and retained diagnostics for an invalid camera
request. The independent installation and direct-client CI step runs
[the public capture check](../scripts/slicer_client_smoke.py) through MCP and
verifies the retained source, camera, image, and provenance identities.
Run the Rust capture tests in
[`slicing::capture::tests`](../crates/printable-server/src/slicing/capture.rs)
for request cancellation and worker behavior.
The [component inventory](../release/components.json) records the packaged
binary, corresponding source, and checksums.

## Headless runtime

The container's `orca-headless` wrapper provides Xvfb and Mesa software rendering
without GPU passthrough. It creates a private runtime directory inside
each slice's temporary staging directory. The worker's existing process-group
cancellation and staging cleanup also cover the display process and runtime files.
The image includes the English locale needed by Orca's GUI initialization. The
wrapper supplies the installed system certificate bundle to prevent an
interactive certificate-selection prompt.
The native smoke checks A1/X1C slices and package re-slicing, including decodable,
nonblank thumbnail relationships, retained geometry, G-code, and build-plate settings.
It also slices for the two-nozzle H2D with two filaments loaded, and checks
process and filament overrides, slot colours and vendor subfolder profiles.

Read `printable://contracts/slice/{action}` for exact request parameters.
For uploaded-model preparation and profile choices, load
[`prepare-fdm-print`](../skills/prepare-fdm-print/SKILL.md) through `skill.get`.
The [preparation guide](fdm-preparation.md) describes the workflow, evidence
behind its decisions, and capabilities that still need native support.
Use `profiles` to find printer, process and filament profiles, and `settings` to
inspect inherited settings before preparing a slice. Profiles come from the BBL
collection, including its vendor subfolders such as Polymaker and the P1P
variants. When a BBL filament inherits from a profile the collection lacks, the
worker reads that parent from Orca's shared `OrcaFilamentLibrary` beside it.

Each selected profile may override any OrcaSlicer 2.4.2 setting in its category.
`settings` lists those settings with the resolved profile value, its originating
profile or explicit override, and whether each one is overrideable. A null profile
value means the profile leaves it unspecified. The separate `definition.default`
reports the native default only when the generator can resolve it from literal
source. It does not establish the final value after job normalization.

Definitions include native types, vector shape, declared units and bounds, enum
choices, and open suggestions where available. `unknown_fields` identifies
unresolved expressions; a definition with `status: unknown` may be generated
dynamically by the native engine. Hardware and job applicability remain unknown
until checked for the selected job. Setting presence does not establish that a
tower or other feature will be generated. The worker refuses
these overrides:

- identity and compatibility settings, such as `inherits`, `printer_model`,
  `nozzle_diameter` and `compatible_printers`; select the matching profile instead
- printer upload connections and credentials, post-processing scripts, and
  custom bed model or texture files
- names that Orca 2.4.2 does not define for the category, including Bambu Studio
  names such as `wall_infill_order`, which Orca would ignore

For process discovery, the optional `printer` selection supplies tower context.
`tower_context` and each relevant row's `applicability` distinguish supported,
inapplicable and unknown generator controls, with pinned source links. BBL selects
Type 1 regardless of configured `wipe_tower_type`. Both implementations consume
rotation through shared integration. Type 1 does not generate a stabilization
cone; Type 2 needs cone walls for that control. Rib planning derives tower width,
so configured width is distinct from measured geometry. Missing process context
cannot establish a Type 2 cone setting's applicability. These are generator facts,
not physical hardware compatibility or proof that a job used a parameter.

Override values are Orca strings or nonempty string arrays. The worker checks
literal numeric bounds and closed enum choices before admitting a slice. Open
suggestions do not restrict numeric values. Complex serialization, unresolved
definitions and combinations retain native validation; a failed slice repeats
Orca's reason. When the engine reports structured warnings, completed status
and retained slice provenance include `settings_substitutions`. In the
`reported` state, `keys` lists the settings the engine replaced and
`truncated` indicates that the key list is incomplete. The `unavailable` state
reports an invalid or oversized native report. Older runs and engines may omit
the field. The retained native result omits raw warning records, and a report
that exceeds the limit or cannot be parsed is replaced with a summary artifact.
Static bounds do not establish suitable printing values.

`catalog_identity` identifies the packaged engine and corresponding source.
`runtime_source_verified: false` means the worker's version probe does not attest
the source revision of an externally supplied executable. The bundled catalog
must match the worker's pinned version and source before profiles load.

Each filament selection may also set `colour`, such as `#0A2CE3`. An AMS tray
value such as `0A2CE3FF` is also accepted. Colours are recorded in the sliced
package and G-code, and Orca recalculates purge amounts and plate thumbnails
when a colour changes. A slot without a colour keeps the source 3MF's colour.
Colour is a slot setting, not a filament profile setting, so a `filament_colour`
override is refused.

`prepare` requires an existing `project_id`, a project-relative STL or 3MF
`source`, a new `output_dir`, printer and process selections, ordered filament
selections, and an explicit physical `build_plate`. Convert STEP through
`cad_build` first. The numbered `plate` selects a 3MF plate (one-based; zero means
all plates); it does not select the build surface. Material ordering must match
the input's filament indices. Slicing is not automatic material assignment.

Preparation snapshots the source and retains resolved settings, progress, logs,
state and output hashes. Poll `status` using the same project and output directory;
do not submit the same operation again after a connection timeout. The worker
admits one preparation or review at a time. `cancel` requests cancellation of
active work. A restarted worker retains completed results and reports unfinished
work as interrupted; it never silently resumes or replays it.

### Source and final layout evidence

`inspect_paint` accepts a project-relative 3MF and returns its source hash and
the support, seam, material and fuzzy-skin paint reported by OrcaSlicer.
Painted volumes use one-based object and volume indices. The category flags
describe the full project, including volumes omitted from the response. A facet
can contribute to the count for more than one paint layer. Large results return
summary counts with a bounded volume list and a truncation flag. Inspection
accepts files up to 128 MiB and rejects native reports above 8 MiB. It uses a
private snapshot and leaves the source unchanged. It runs a supported engine
without a display and limits the child process to a 1 GiB address space.

`inspect_layout` accepts an authored project-relative 3MF, a new output directory
and optional expected source SHA-256. It resolves core/production model resources
without extracting archive members, and reports build indices, native instance
and plate identities, build/component transforms, bounds, material settings and
solid interference. The complete report is a retained artifact with a SHA-256;
the response includes at most 32 instances and findings with total counts.
Millimetre mesh projects are supported. Unknown required extensions, modifier
semantics, units or resource paths remain explicit limitations.
The complete report retains declared object-level `pid`/`pindex` defaults and
referenced property groups/indices. These core 3MF references are not inferred
printer filament slots.

Source copying runs off the async executor and checks cancellation between chunks.
The inspection deadline covers copying and geometry work. Cancelling the public
call before publication stops the geometry child and releases its admission;
once report publication begins, retention finishes as a completion boundary.

`prepare.layout` binds edits to `source_sha256`. `placements` contains zero-based
`build_index` and absolute `transform_3mf` values from inspection. The 12 values
are the three linear columns followed by XYZ translation. Edits must preserve
the source transform's intrinsic scale and handedness. Unlisted instances stay
unchanged. `tower_positions` contains one-based native `plate` and bed-local
`position_mm: [x,y]`. It edits existing native coordinate vectors; it cannot
invent plate identities. `minimum_gap_mm` requests source-solid separation,
independently of generated support/brim expansion. Explicit layout controls
require `auto_arrange: false` and `auto_orient: false`.
`intended_interfaces` can carry distinct authored `build_indices: [a,b]` and a
design `reason` under the same source hash. Their actual measured interference
and stated intent remain in the report. Final inspection maps them through
stable native identities; an uncertain mapping stays unverified and cannot remove
a measured conflict finding. They do not suppress tower, bed or
machine checks. Do not declare an unintended overlap to be a design interface
merely to clear a finding.

Preparation retains `source-layout.json`, an edited `prepared.3mf` when requested,
and `final-layout.json` with source/package/report hashes. Final verification
requires stable nonzero native instance identities, compatible coordinate frames,
oriented geometry signatures on a declared 0.0001 mm grid, transforms and plates.
Bounds alone do not establish preservation. A changed known placement is a
finding; an ambiguous mapping or native geometry normalization is unmeasured.
Requested tower positions also compare final native coordinate settings. A
changed coordinate is a finding; matching settings alone leave the geometric
effect unmeasured. Inspect the generated tower paths and retained footprints.
Requested automatic moves of known unlocked instances are retained as observed
changes. Locked placements remain required; missing lock or identity evidence
leaves changed placements unmeasured. Final exported solid conflicts and failed
preservation block aggregate preflight even if Orca succeeds. An original source
conflict is considered corrected by automatic layout only when stable identities,
geometry and the final solid check verify that correction. Unknown checks
preserve the files and unresolved result.

The private edit copies unrelated ZIP members raw and changes only selected build
transforms and tower coordinate values. Modified known entries retain file
options but may not retain every ZIP extra field. Unknown native metadata remains
in the original/private source; its preservation through Orca's export, painted
masks and stable face identity are explicitly unverified. Allowlisted local
extruder and support-interface assignments appear in both source/final reports
for inspection; resolved global profiles do not replace that evidence.
`material_assignment_preservation` explicitly remains unverified after native
export; inspect corresponding assignments and actual material/interface paths.

Exact source operations run in a disposable geometry process. The slicer worker
honors `PRINTABLE_GEOMETRY_WORKER_BIN` and
`PRINTABLE_GEOMETRY_WORKER_MEMORY_MIB`; inspection budgets also bound XML bytes,
expanded instances, pairs and time. A timed-out child is killed. Generated
deposition, toolhead sweeps and bed exclusions remain separate final-G-code
checks with their own coverage and assumptions.
Inline preflight state is bounded to 32 plates and 32 findings, footprints and
separations per plate. Counts and `*_inline_truncated` identify omitted inline
evidence; read the complete report artifact and verify its SHA-256. These flags
are separate from `findings_truncated`, which means analysis itself stopped at
its finding budget and coverage remains incomplete.

Terminal states record `finished_unix_ms`. When a slice completes, the worker
also writes `summary.json` next to the outputs, and `status` returns it as
`summary`: finish time, build plate, printer and process, the profile names
stored in the package, the sliced plates and plates with thumbnails, and each
filament slot's profile, type and colour. A colour left to the source 3MF is
read from the sliced package; `filaments_truncated: true` means the package
listed more slots than the summary reports. Writing the summary never changes
the slice result: if it fails, the slice still completes without one. Slices
completed before summaries existed have no `summary.json`; background
consumers summarise them from the package, with the finish time estimated from
`state.json` and `finished_estimated: true`.

A failed slice repeats OrcaSlicer's own message and return code when OrcaSlicer
records a reason. A failed exit without a recorded reason is reported as a likely
crash. `build-log.json` keeps OrcaSlicer's output in both cases.

The caller's MCP client owns its end-to-end request deadline. Worker forwarding
imposes no independent fixed HTTP timeout. A lost response or client deadline
does not establish whether admitted work completed; inspect the same retained
handle before deciding what to do next. Native work uses its caller-selected
processing budget.

Completed artifacts include G-code and a `.gcode.3mf` package. Download them using
the shared `artifact` workflow and verify their recorded hashes. `review` reads
an exact G-code artifact from a completed slice, verifies its hash and renders a
selected layer range as a PNG with retained metadata. These are actual toolpaths,
not a Blender beauty render. The preview supports the implemented XY motion and
arc commands; unsupported modes fail rather than yielding a misleading image.
XYZ origin resets retain physical placement, while extruder resets remain
independent. Excluded arcs update position without generating preview samples.
Parsing and drawing use the caller's positive `timeout_seconds` processing
budget, defaulting to thirty seconds; native encoding is checked before and
after it runs. Full-file layer counts and estimates remain available when
processing completes within that budget.
It does not establish adhesion, strength, clearance or a successful physical print.

## Collision findings

Preparation checks the generated G-code even when Orca exits successfully. The
completed state returns `preflight`, and `preflight.json` retains the same report
as a hashed artifact. A completed slice can have `preflight.status: "blocked"`:
the native files exist, but the checks found modeled interference.

The report checks generated line envelopes against the final package's bed
boundary and exclusion polygon, detects tower/part and tower/support interference,
and checks explicit linear machine motion against geometry already deposited at
that point in the job. Execution order matters for sequential printing and descent
to a lower tower. An intermediate obstruction is detected even when the travel's
endpoints are clear. Generated width and height annotations supply the deposited
envelopes; missing dimensions do not become zero-width geometry.
This includes supported startup deposition before the first layer marker.
Unclassified priming, stationary extrusion and unsupported startup firmware motion
remain unresolved rather than disappearing from the coverage report.

`prepare.clearance.machine_envelopes` accepts occupied component boxes relative
to the active nozzle's G-code reference point. Supply each component's name,
`minimum_mm`, `maximum_mm` and the `source` of those dimensions. Include the gantry
where relevant. Use actual manufacturer or measured geometry; the worker does
not guess a machine envelope from a generic radius. An empty envelope list leaves
toolhead clearance unmeasured. The positive `clearance.timeout_seconds` budget
applies to analysis after native slicing and defaults to thirty seconds.

When known, `clearance.initial_position` supplies `position_mm` and its `source`
for the nozzle before the first command, in the input G-code frame. Omit it when
that position is unknown. The parser does not infer a starting position from
zero coordinates, and a homing command's physical motion is not simulated.

Each plate reports checks as `passed`, `failed`, `unmeasured` or `unsupported`.
Findings identify the source G-code line, layer, material, available object
identity, affected component or earlier deposited path, and position in
millimetres. The report retains a finding per kind/object pair/component, including
each affected pair when one move crosses several objects.
`blocked` takes precedence over incomplete analysis, so a later unsupported
command does not erase an earlier finding. `unresolved` means required evidence
is missing; `modeled_checks_passed` applies only to the stated model and coverage.
Inspect `processing_complete`, `coverage_complete` and unresolved reasons.

Bed geometry comes from `Metadata/project_settings.config` in the final native
package. Invalid or unavailable settings leave that check unmeasured. The report
records G-code hashes and the supplied machine geometry, and slice provenance
binds it to the source, resolved settings and generated artifacts. Re-slice and
recheck after a layout, material, settings, machine-envelope or output change.

These checks use conservative rectangular line envelopes and translating
component boxes. Intended support/brim contact and material interfaces are not
tower collisions. Source-object solid intersections, rounded bead shape, arc
clearance and firmware macro motion are outside this implementation's established
coverage. Firmware conditionals leave subsequent motion/tool/extrusion state
unverified: later explicit XYZ alone does not restore that history. Preflight
retains earlier findings and marks the affected remainder unsupported.
An absolute extrusion move without a known E origin also leaves that material's
retraction debt uncertain. Later numerical E resets do not establish physical
extrusion; those moves cannot create definite deposited obstacles. Each analysis
retains at most 512 collision findings, then stops with
`findings_truncated: true` and incomplete coverage. Established positive findings
remain blocked, and omitted checks never become passes.
Unsupported motion remains explicit; the preview's sampled arcs do not certify
clearance. Warping, loose components and nozzle buildup can cause physical
contact outside the model. No collision report establishes adhesion, strength or
successful physical printing.

Keep prepared artifacts when checks are blocked or unresolved. The preparing
agent must flag findings, correct supported layout/settings problems and recheck,
or withhold a print-ready recommendation. Physical printer control retains its
existing authorization requirements.

Completed slices and reviews return retained provenance references. Optional
`design_record` links a CAD measurement to the exact source bytes. Pass slice and
review references to printer import to check their applicability before upload;
see [design and print evidence](delivery-evidence.md).

`slice.compare` accepts a baseline and one to eight completed candidates with
exact handles and retained slice provenance, rationale, optional review
provenance and declared calibration/material/environment conditions. Choose
`minimum_material_support`, `strength_in_use` or `finish_named_faces`; record
named `critical_faces` and `load_assumptions` when relevant. Supply a new
`output_dir`. The response returns a comparison artifact, SHA-256 digest and
retained comparison provenance for restart/readback through artifact operations.
It does not create another slice job or select a winner.

Comparison verifies retained source snapshots, native outputs, settings
identities and applicable review/media bytes. It records exact requests,
transforms, source/final layout references and collision scope. Source changes,
different printer/nozzle/materials/build surface/engine, shell-setting changes,
and missing or changed physical conditions are explicit compatibility flags.
Historical evidence remains applicable to its original bytes. Physical
equivalence and strength remain unverified. Global resolved inputs are distinct
from native per-object settings. Native G-code estimate strings retain units
and ordered material slots; unsupported support/tower/flushing breakdowns are
null. Parsing is bounded to 30 seconds, 512 MiB of inputs (including conservative
record charges), 4 MiB per retained record and 64 KiB per toolpath line. Larger
comparisons require smaller completed candidates, not truncated conclusions.

The worker accepts source artifacts and nonempty material-profile lists without
fixed product capacity ceilings. Preparation uses a positive caller-selected
timeout. Toolpath reviews accept ordered layer ranges, feature selections, and
positive image dimensions without fixed count ceilings; their caller-selected
`timeout_seconds` defaults to thirty seconds. Container memory, CPU and temporary
storage controls remain separate runtime boundaries. Neither preparation nor review uploads a
job to a printer or starts physical work.

## Native layout capture

`slice.capture` observes an existing Orca project or the exact package produced by
a completed preparation. It renders the native model layout without arranging,
slicing, uploading or starting a print. The input package and completed
preparation remain unchanged.

For example, call the `slice` tool with:

```json
{
  "action": "capture",
  "params": {
    "project_id": "enclosure",
    "source": {"kind": "prepared", "output_dir": "slice"},
    "camera": {"preset": "isometric", "projection": "orthographic"},
    "width": 1600,
    "height": 1000,
    "timeout_seconds": 300
  }
}
```

Use `{"kind":"project","path":"layout.3mf"}` for a project-relative source.
Prepared capture verifies the recorded digest of `model.gcode.3mf` and retains
the preparation's provenance. It does not reopen the original model before
Orca arranged it.

The presets are `isometric`, `top`, `front`, `rear`, `left`, `right` and `bottom`.
Each uses the same native camera implementation. `camera.orientation` accepts a
quaternion in `[w,x,y,z]` order and overrides the preset orientation.
`camera.target` sets a workspace point in millimeters; `camera.zoom` sets native
zoom. An explicit zoom outside the layout's native camera range is rejected.
Orthographic and perspective projections are supported. Automatic framing
uses a margin of `1.1` by default; the margin must be at least one. Explicit
target or zoom controls can crop the view. The returned `camera` contains the
resolved controls and can be passed back unchanged to repeat that view over the
same source and selection.

`plates` and `objects` contain one-based source indices. Empty lists select all;
object selection applies within the selected plates. Plates keep their native
relative positions. `visibility.beds`, `visibility.plate_identifiers` and
`visibility.prime_towers` default to true. Image dimensions and the processing
budget must be positive and representable by the runtime. Graphics limits,
container resources and the caller's request deadline still apply.

The processing budget covers admission, native startup, rendering and image
validation. Cancellation stops this processing, including through public MCP.
Storage publication begins only for a validated observation before its budget
expires. Once publication begins, retention completes and returns its artifact
references; a processing timeout cannot turn a published observation into an
unreported result. Artifacts already being retained may remain after a caller
cancels the request.

The result identifies `kind: native_orca_layout`,
`representation: model_placement` and `prime_towers: native_estimated`.
These are Orca's native model and estimated prime-tower volumes, including when
the source package contains sliced output. Use `slice.review` to inspect actual
toolpaths. A layout capture does not establish print safety or clearance.

The PNG, native metadata and observation metadata are retained under the
project's `observations/` directory. The response includes the source digest,
engine identity, selected plates and objects, resolved camera, visibility,
image dimensions, artifact digests and an immutable observation provenance
reference. Download artifacts with the `artifact` workflow. The provenance also
retains an immutable copy of the image.

Capture requires an Orca build that advertises `--capture-workspace`.
Worker readiness reports this as `native_capture`; a compatible engine can
still lack a usable graphics context. Both missing native support and unsupported
graphics return `native_capture_unavailable`. Other native failures retain a
diagnostic artifact. Capture never substitutes a stored thumbnail or a composite
of plate images for the requested native view.

## Plate previews

The worker also exports OrcaSlicer's plate picture of a model without slicing
it, for [generated project pictures](project-library.md#generated-pictures).
The server sends `POST /preview` to the worker with a project and a
project-relative STL, OBJ or 3MF. The worker runs OrcaSlicer with
`--export-3mf` and no `--slice`, placing a bare mesh on the plate with
`--arrange 1` while a 3MF keeps its own arrangement, and answers with the
exported `Metadata/plate_1.png`. A preview may take five minutes. It is not an
action of the `slice` tool.

Previews are background work. One is refused with HTTP 409 while any other
operation holds the slicer, and the server makes it in a later pass. A slice or
toolpath review requested while a preview runs waits for the preview to finish
instead of being refused as busy.

## Updating the packaged Orca release

The setting names and generated definitions live in
`crates/printable-server/src/slicing/orca-settings.json`, generated from the
Orca source at the packaged release. Install `requirements-tooling.txt` in the
isolated contributor environment first. When the release changes, regenerate the
file from a clean checkout of that revision and review the difference:

```sh
python3 scripts/orca_settings.py /path/to/OrcaSlicer \
  --engine-version 2.4.2 \
  --source-revision FULL_COMMIT_SHA \
  --output crates/printable-server/src/slicing/orca-settings.json
```

The generator checks the checkout's full revision and rejects modified definition
files. It parses C++ syntax without executing the source. Conditional values,
dynamic keys, ambiguous definitions and unresolved defaults stay explicit.
The catalog must match the worker's pinned version and source revision; update
both with the packaged executable and corresponding-source notice. A test fails
when a new setting name looks like a printer
connection, credential or script setting but is not on the refused list.
