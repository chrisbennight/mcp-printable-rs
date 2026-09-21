# Printer observation and physical printing

Modeling, rendering and inspecting files do not require a printer or physical
readiness answers. Printer integration is optional. Use `printer` for discovery,
selected status sections, materials, history and project camera artifacts. Use
`print` for prepared-file import, review, staged jobs, records and physical control.
Read a selected action schema through `printable://contracts/{tool}/{action}`.

## Physical setup

At the start of each physical printing session ask which build plate and
upward-facing surface are installed on each target printer, and whether the bed
is clear. Reuse explicit answers already provided for the current session.
Authorization to print is separate from these necessary physical setup facts.
An idle/completed status or camera image does not establish the installed surface
or an empty bed. Confirm part removal before another print. Occupancy does not
prevent modeling, reviewing or preparing files.

Match the installed nozzle, requested filament product/variant and physical
surface to the prepared slice. Missing nozzle or material identity requires
clarification; do not assume a nozzle diameter or infer transparency from color.
Changing print options cannot correct temperatures or start G-code already sliced
for a different surface. Review the toolpath with the slicer that produced it.

## Review, stage and start

Import a project-relative printer-ready `.gcode.3mf`. Import does not print.
Review the selected source, plate and target with `print.review`; inspect known,
unknown and mismatched material, nozzle and model information. Default responses
are compact; request selected sections or detail only when needed. Review is an
observation, not a reservation or a guarantee of physical safety.

Stage explicitly with the target printer, project plate and ordered AMS mapping.
Use reported tray IDs rather than assuming the printer's display labels or a
particular AMS variant. External spools use `use_ams: false` and the target's
supported mapping. Select calibration and timelapse options deliberately for the
hardware and plate. Staging requests manual start and returns its effective setup.

Inspect status before control, and start only with current physical readiness
answers and an appropriate slice. Bambuddy owns scheduling and authoritative
dispatch checks. Start acknowledgement is not print completion. Use printer
status for physical observations and print status/history for job records.
`clear_plate` acknowledges physical removal and may release queued automatic
prints; never invoke it just to clear a software flag. Resume may cause motion.

## Failures and recovery

A delivered mutation can have an unknown outcome. Inspect the printer and job
before deciding whether another action is needed; do not automatically replay
start, upload, stage or control calls after transport uncertainty. Read requests
may retry one transient failure. Typed backend rejections preserve bounded,
sanitized diagnostic details without exposing raw response bodies.

`FAILED` records a previous-job outcome and can remain valid when a replacement
is allowed by Bambuddy's dispatch checks. Do not wait for a fabricated transition
to `IDLE`. Confirm removal, review the retained source and stage a new job.
Reported diagnostic codes alone do not establish safety or a dispatch prohibition;
use supported meanings/actions and report missing information rather than guessing.

`printer.refresh_status` requests full telemetry but does not prove it was
received. Its `fresh_status_observed: false` and retrieval timestamp must not be
presented as device observation time. Read status separately. Error clearing is
an explicit `print.control` action, not automatic recovery or plate clearance;
acceptance does not prove that the printer resolved the condition.

Caller authorization belongs to the gateway or authenticated MCP client boundary.
Backend service credentials remain internal; they are not a second caller
permission system. Tool-wide write annotations cover mixed actions and are not
action-specific authorization rules.
