# Optional printer integration

For the complete path through OrcaSlicer preparation to sending and starting a
print, see [Slice, review, and send a print](workflows/model-to-printer.md).
This page covers configuring Bambuddy and interpreting printer results.

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
Optional import evidence links the exact local upload to its slice and toolpath
review. Retained submission and queue observations distinguish accepted requests
from observed execution, while backend digest verification remains unverified.
See [design and print evidence](delivery-evidence.md) for reference fields,
historical records, and recovery after an unknown outcome.
The workflow resource explains plate readiness, calibration, AMS mapping and
replacement-print handling. Printable's [native slicer](slicing.md) prepares
and reviews the printer-ready file before these delivery operations.

## Print outcome records

While a backend is configured, the server follows each print that Printable
staged or started until the backend reports `completed`, `failed`, `cancelled`
or `skipped`, then writes one record to
`.printable/prints/<project_id>/<print_id>.json`. The record keeps the outcome,
the backend's status, printer identity and model, plate, AMS mapping, filament
type and colour as the backend reports them, start and completion times, any
error message, and references to the submission records. It is written once and
does not depend on the backend keeping its own history; back it up with the
rest of `.printable`.

| Setting | Default | Meaning |
| --- | --- | --- |
| `PRINTABLE_PRINT_FOLLOW_INTERVAL_SECONDS` | 60 | Seconds between passes, up to 3600; `0` turns following off |
| `PRINTABLE_PRINT_FOLLOW_MAX_HOURS` | 168 | Hours after submission, 1 to 8760, after which an unfinished print is recorded as `unknown` |

A print the backend no longer has, or one that is still unfinished after the
follow time, is recorded with outcome `unknown` and a reason; Printable never
guesses success or failure. A failed backend read is retried on the next pass
until the follow time ends; after that the print is recorded as `unknown`.
Only prints staged through Printable from a file that Printable uploaded with
`print import` are followed. Prints started from other software, prints staged
from a print archive or from a library file Printable did not upload, a staging
request whose outcome was unknown, and additional copies from a request with a
quantity above one have no submission link and are not recorded. The link is
written just after the submission record, so a server that stops between the
two leaves that print unfollowed. A print whose link or submission record
cannot be read is skipped with a warning in the server log, and other prints
are still followed. When a print was staged and later started, the most recent
acceptance supplies its printer and setup details.

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
