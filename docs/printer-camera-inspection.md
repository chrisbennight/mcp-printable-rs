# Printer camera inspection skill

Printable bundles [inspect-printer-camera](../skills/inspect-printer-camera/SKILL.md)
for an MCP client to run with its vision model. It reviews visible evidence of
spaghetti, collapsed or displaced structures, lifted geometry, nozzle buildup,
and collision risk. A normal camera view can remain useful when the toolhead
hides the nozzle or top surface. Those blind spots alone do not require a warning.

## Discover and load

Call the `skill` tool with these arguments to list compact metadata:

```json
{"action":"list","params":{}}
```

Load the selected skill with:

```json
{"action":"get","params":{"name":"inspect-printer-camera"}}
```

The result contains `skill` metadata and the complete `instructions`. Metadata
includes the name, description, resource URI, and lowercase hexadecimal SHA-256
of the exact UTF-8 instructions. Retain the digest with assessment records to
identify the instructions used; loading a later server release may change it.

Clients supporting MCP resources can discover the same Markdown in
`resources/list` and read it with `resources/read`:

```json
{"uri":"printable://skills/inspect-printer-camera/SKILL.md"}
```

The tool uses standard MCP `tools/list` and `tools/call`; the resource uses
standard `resources/list` and `resources/read`. There is no custom MCP skills
protocol or installation requirement. Both paths return the bundled file,
including its Agent Skills frontmatter. Discovery and loading work without
configured printer integration and perform no camera capture or print control.

## Run a check

After loading the instructions, the parent agent resolves the requested printer,
reads its status, obtains a project camera artifact using `printer.snapshot`,
and publishes it using `artifact.publish`. Transfer and verify the image through
the client's file mechanism, then open its actual pixels. A supplied image can
be inspected directly without contacting a printer. See
[printer integration](printer-integration.md) for the existing interfaces.

When delegation is available, the parent gives a vision-capable sub-agent the
image and the skill's assessment instructions. The sub-agent returns visible
evidence, coverage, confidence, limitations, and a recommendation. It has no
control or messaging responsibility. The parent checks the result, handles
authorization and current printer context, and sends any notification or
authorized pause. Clients without sub-agents use the same assessment with their
vision model and report that fallback.

The decisions are `no_issue_observed`, `notify_user`, `pause_and_notify`, and
`unable_to_assess`. The last means the view is broadly unusable, rather than
merely lacking a visible nozzle. No issue observed means no convincing failure
in the visible regions; it does not certify hidden geometry or bed readiness.

The default is assessment with a recommendation. Existing user authorization
or a monitoring policy must cover any pause. Printer-level pause cannot enforce
a restriction to a particular job, so automatic pausing needs the printer-wide
authority described in the skill. Accepted commands and observed paused state
remain separate. An uncertain delivered pause is inspected rather than retried;
the skill never automatically resumes a print.

## Repeated monitoring

Loading this skill does not start a monitoring service. A client runtime owns
the authorized schedule, vision model, sub-agent support, notifications, and
durable incident state. Automatic control needs serialized checks per printer
and incident records that survive interrupted invocations. Without those
facilities, use assessment and notification without automatic pausing.

Scheduled checks retain assessment data separately from user notifications.
An unchanged normal result has `notification_required: false`; ordinary
toolhead occlusion alone is not an incident. Unchanged warnings and uncertain
pause attempts must not be replayed by a later check.

## Evidence and limits

MCP regression tests cover discovery, exact instruction retrieval, metadata
integrity, invalid requests, and availability without printer integration. A
manual vision sub-agent check of a normal chamber photograph returned
`no_issue_observed` with moderate confidence for visible regions despite a
hidden nozzle and top surface. This is a limited usability check, not a measured
failure-detection accuracy benchmark. The bundled instructions document the
evidence and authorization needed before intervention.
