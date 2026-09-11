# Security

Printable is intended for one trusted person or team. A bearer gives access to
the shared workspace and live scene, including deliberate Python execution
inside Blender. Do not give it to untrusted users. Containers limit access to
host resources; they do not turn Python into a language sandbox.

Keep MCP on loopback or behind a trusted HTTPS proxy. Do not publish the Blender
bridge, mount a Docker socket, use privileged containers or host PID mode, or
mount unrelated host data. Keep credentials out of Blender containers. Exact
Origin and Host allowlists supplement authentication; they do not replace it.

## Report a vulnerability

This repository is currently private. Existing collaborators should report a
suspected vulnerability in a restricted repository issue and avoid including
live secrets. Include affected revisions, a minimal reproduction, impact, and
any mitigation already applied.

Before public access is enabled, maintainers must enable GitHub private
vulnerability reporting and verify the **Security → Report a vulnerability**
path with an external account. That public reporting path is not claimed to be
available yet. Do not post vulnerability details in a public issue. If no
verified private reporting path is available, do not submit sensitive details;
maintainers must establish that path before public release.

Only the current maintained revision is covered by fixes at this stage; there
is no long-term support branch or promised response deadline. Security reports
are assessed for the actual trusted-team deployment boundary. Deliberate
authorized Python execution is a product capability; escaping its container,
unauthorized file access, credential disclosure, or bypassing transport
authentication is a security defect.

## Release and incident handling

Use matching immutable image digests and retain the release's source revision,
scan results, and qualification evidence. A software-rendered CI pass is not a
production NVIDIA qualification. Review known unremediated findings before
using a pair; passing the configured scan policy does not mean an image has no
vulnerabilities.

If a bearer leaks, stop remote access and rotate it through a protected local
process, then recreate the server and update clients. Removing a value from the
latest commit does not revoke it. Preserve relevant redacted evidence for
investigation and review the scope of workspace access. Restore data from a
trusted backup when its integrity cannot be established.
