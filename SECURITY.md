# Security

Printable is intended for one trusted person or team. A bearer gives access to
the shared workspace and live scene, including deliberate Python execution
inside Blender. Do not give it to untrusted users. Containers limit access to
host resources; they do not turn Python into a language sandbox.

Keep MCP on loopback or behind a trusted HTTPS proxy. Do not publish the Blender
bridge, mount a Docker socket, use privileged containers or host PID mode, or
mount unrelated host data. Keep credentials out of Blender containers. Exact
Origin and Host allowlists supplement authentication; they do not replace it.

The optional [model library](docs/model-library.md) saves files from public
websites. Treat those files and their titles and descriptions as untrusted
input, like any uploaded file.

## Report a vulnerability

This repository is public. Do not report sensitive vulnerability details or
live secrets in public issues. A private report should include affected
revisions, a minimal reproduction, impact, and any mitigation already applied.

GitHub private vulnerability reporting is enabled for this repository. Open
the [Security page](https://github.com/chrisbennight/mcp-printable-rs/security)
and choose **Report a vulnerability** to submit a private report. Do not post
vulnerability details in a public issue. If the reporting route is unavailable,
do not submit sensitive details through a public channel.

Only the current maintained revision is covered by fixes at this stage; there
is no long-term support branch or promised response deadline. Security reports
are assessed for the actual trusted-team deployment boundary. Deliberate
authorized Python execution is a product capability; escaping its container,
unauthorized file access, credential disclosure, or bypassing transport
authentication is a security defect.

## Release and incident handling

Use matching immutable image digests and retain the release's source revision,
scan results, and CI results. Software-rendering tests do not measure GPU
performance or driver compatibility. Review known unremediated findings before
using a pair; passing the configured scan policy does not mean an image has no
vulnerabilities.

If a bearer leaks, stop remote access and rotate it through a protected local
process, then recreate the server and update clients. Removing a value from the
latest commit does not revoke it. Preserve relevant redacted evidence for
investigation and review the scope of workspace access. Restore data from a
trusted backup when its integrity cannot be established.
