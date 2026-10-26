# Native slicing

The `slice` workflow uses a separate, credential-free OrcaSlicer worker. It shares
the project workspace with the server and other engines, but cannot control a
printer. The packaged worker uses OrcaSlicer 2.4.2 and its BBL profile collection.
Profile availability is not a claim of compatibility with every printer.

The checksum-pinned [fork release](https://github.com/chrisbennight/OrcaSlicer/releases/tag/local-2.4.2-b08db1a)
supports headless package thumbnails by fixing the CLI OpenGL context request and
model initialization without a wx application. It also carries three upstream
fixes from the 2.4.4 backports: bounds checks for per-filament settings lists
shorter than the filament count, which crashed H2D slices with two or more
filaments; bounds checks for tool-change lookups; and two buffer overflow fixes
in the STL reader. It retains the upstream 2.4.2 JSON profile format used by the
worker.

The container's `orca-headless` wrapper provides Xvfb and Mesa software rendering
without GPU passthrough. It creates a private runtime directory inside
each slice's temporary staging directory. The worker's existing process-group
cancellation and staging cleanup also cover the display process and runtime files.
The native smoke checks A1/X1C slices and package re-slicing, including decodable,
nonblank thumbnail relationships, retained geometry, G-code, and build-plate settings.
It also slices for the two-nozzle H2D with two filaments loaded, and checks
process and filament overrides, slot colours and vendor subfolder profiles.

Read `printable://contracts/slice/{action}` for exact request parameters.
Use `profiles` to find printer, process and filament profiles, and `settings` to
inspect inherited settings before preparing a slice. Profiles come from the BBL
collection, including its vendor subfolders such as Polymaker and the P1P
variants. When a BBL filament inherits from a profile the collection lacks, the
worker reads that parent from Orca's shared `OrcaFilamentLibrary` beside it.

Each selected profile may override any OrcaSlicer 2.4.2 setting in its category.
`settings` lists those settings with the profile's value, or null when Orca's
default applies, and marks whether each one is overrideable. The worker refuses
these overrides:

- identity and compatibility settings, such as `inherits`, `printer_model`,
  `nozzle_diameter` and `compatible_printers`; select the matching profile instead
- printer upload connections and credentials, post-processing scripts, and
  custom bed model or texture files
- names that Orca 2.4.2 does not define for the category, including Bambu Studio
  names such as `wall_infill_order`, which Orca would ignore

Override values are Orca strings or nonempty string arrays. The worker does not
check them against Orca's allowed values; Orca rejects an invalid value when it
slices, and the failed slice repeats Orca's reason.

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

Completed slices and reviews return retained provenance references. Optional
`design_record` links a CAD measurement to the exact source bytes. Pass slice and
review references to printer import to check their applicability before upload;
see [design and print evidence](delivery-evidence.md).

The worker accepts source artifacts and nonempty material-profile lists without
fixed product capacity ceilings. Preparation uses a positive caller-selected
timeout. Toolpath reviews accept ordered layer ranges, feature selections, and
positive image dimensions without fixed count ceilings; their caller-selected
`timeout_seconds` defaults to thirty seconds. Container memory, CPU and temporary
storage controls remain separate runtime boundaries. Neither preparation nor review uploads a
job to a printer or starts physical work.

## Updating the packaged Orca release

The overrideable setting names live in
`crates/printable-server/src/slicing/orca-settings.json`, generated from the
Orca source at the packaged release. When the release changes, regenerate the
file from a checkout of that release and review the difference:

```sh
python3 scripts/orca_settings.py /path/to/OrcaSlicer \
  --engine-version 2.4.2 \
  --source-revision FULL_COMMIT_SHA \
  --output crates/printable-server/src/slicing/orca-settings.json
```

A unit test fails until the file's engine version matches the worker's pinned
version. Another test fails when a new setting name looks like a printer
connection, credential or script setting but is not on the refused list.
