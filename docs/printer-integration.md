# Optional printer integration

Printable exposes the `printer` and `print` workflows through the same
authenticated MCP endpoint as modeling. Bambuddy provides device observations,
library records, materials, queue management and physical control. It remains
responsible for authoritative dispatch and scheduling.

Set `PRINTABLE_BAMBUDDY_URL` to the backend origin and supply
`PRINTABLE_BAMBUDDY_READ_KEY` and `PRINTABLE_BAMBUDDY_CONTROL_KEY` through your
deployment's secret provider. Both are service credentials; they may contain the
same authorized key. They do not define caller permissions. Do not place keys in
URLs, commands, committed configuration, prompts or logs. No network request is
made while parsing settings. When no endpoint is configured, modeling continues
and printer calls report that the integration is not configured.

The endpoint must have no credentials, query, fragment or non-root path. HTTPS is
supported; HTTP is restricted to private IP addresses, loopback or single-label
service names. Redirects and ambient proxy settings are disabled. The server
requires network access to this configured service; the Blender and CAD workers
do not receive these credentials. Deployment-specific network wiring belongs in
the operator's configuration, not this repository.

## Discovery and responses

Read `printable://printing/workflow-v1` before physical printing. Discover a
selected action using `printable://contracts/printer/status` or
`printable://contracts/print/review`. Typed inputs and output schemas preserve
known backend fields without forwarding arbitrary requests or raw bodies.
Status defaults to a compact summary. Selected status sections, record detail
and paginated materials/history provide richer information on demand. Unknown
remaining material and missing fault meanings remain unknown, not fabricated.
Camera images are confined project JPEG artifacts, not inline encoded payloads.

Import accepts a snapshotted project-relative printer-ready `.gcode.3mf`. Review
compares the selected source, plate, target, materials and hardware observations.
Stage requests manual start; the explicit start/control actions are separate.
Neither review nor acknowledgement certifies physical safety or completion.
The workflow resource explains plate readiness, calibration, AMS mapping and
replacement-print handling. Slicing itself is a separate capability; use a
compatible slicer to prepare and visually review the printer-ready file.

## Failure semantics

Reads retry one transient failure. Mutations are sent once. Transport failure or
an invalid success response after sending a mutation returns
`printer_outcome_unknown`; inspect the actual backend state before further action.
Recognized rejections return `printer_rejected` with bounded typed diagnostic
details, not the raw response. Telemetry refresh confirms a request, not receipt
of fresh device observations. Clearing diagnostic errors does not acknowledge
physical bed clearance.

Unit and MCP tests use isolated fake services and must not trigger real hardware.
Actual deployment acceptance requires checking the served contracts and backend
compatibility separately; passing local tests is not a claim of live integration.
