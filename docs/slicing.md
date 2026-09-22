# Native slicing

The `slice` workflow uses a separate, credential-free OrcaSlicer worker. It shares
the project workspace with the server and other engines, but cannot control a
printer. The packaged worker uses OrcaSlicer 2.4.2 and its BBL profile collection.
Profile availability is not a claim of compatibility with every printer.

The checksum-pinned [fork release](https://github.com/chrisbennight/OrcaSlicer/releases/tag/local-2.4.2-6915edb)
supports headless package thumbnails by fixing the CLI OpenGL context request and
model initialization without a wx application. It retains the upstream 2.4.2 JSON
profile format used by the worker.

The container's `orca-headless` wrapper provides Xvfb and Mesa software rendering
without GPU passthrough. It creates a private runtime directory inside
each slice's temporary staging directory. The worker's existing process-group
cancellation and staging cleanup also cover the display process and runtime files.
The native smoke checks A1/X1C slices and package re-slicing, including decodable,
nonblank thumbnail relationships, retained geometry, G-code, and build-plate settings.

Read `printable://contracts/slice/{action}` for exact request parameters.
Use `profiles` to find printer, process and filament profiles, and `settings` to
inspect inherited settings before preparing a slice. Each selected profile may
carry supported overrides; identity and nozzle overrides are refused.

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

Completed artifacts include G-code and a `.gcode.3mf` package. Download them using
the shared `artifact` workflow and verify their recorded hashes. `review` reads
an exact G-code artifact from a completed slice, verifies its hash and renders a
selected layer range as a PNG with retained metadata. These are actual toolpaths,
not a Blender beauty render. The preview supports the implemented XY motion and
arc commands; unsupported modes fail rather than yielding a misleading image.
XYZ origin resets retain physical placement, while extruder resets remain
independent. Excluded arcs update position without generating preview samples.
Parsing and drawing have a cooperative 30-second processing deadline; bounded
native encoding is checked before and after it runs. Full-file layer counts and
estimates remain available when processing completes within that budget.
It does not establish adhesion, strength, clearance or a successful physical print.

The worker accepts source artifacts up to 1 GiB, one to sixteen material profiles
and a preparation timeout up to two hours. Container memory, CPU and temporary
storage limits remain separate limits. Neither preparation nor review uploads a
job to a printer or starts physical work.
