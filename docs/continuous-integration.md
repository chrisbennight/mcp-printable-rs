# Continuous integration

GitHub runs the [CI workflow](../.github/workflows/ci.yml) on pull requests,
pushes to `main`, and manual requests. All jobs use disposable GitHub-hosted
Linux/amd64 runners. They need no repository secrets, private registry, lab
network, or GPU host. Dependencies resolve from their public upstreams.

| Check | Evidence |
| --- | --- |
| Source checks | Rust formatting, lint, locked tests; Python bridge and release-policy tests; documentation and shell validation |
| Workflow validation | GitHub Actions syntax and expression checks with pinned actionlint |
| Confined-source fuzz smoke | A bounded run against the OpenSCAD source gate with pinned tooling |
| Container integration (software graphics) | Both built images, packaged notices, standalone installation and verified downloads, workspace restoration and storage failures, Blender shutdown recovery, native views, and checkpoint rendering while live editing continues |

The container job uses the same Dockerfiles and product smoke as release
qualification. It builds the smoke driver in the server's build environment;
the driver is not shipped in the runtime image. Local image tags are temporary
test references, not releases. No job publishes images or triggers deployment.

The supported deployment target remains Linux/amd64 with NVIDIA. Software
graphics lets CI exercise Blender without giving pull requests access to a GPU
runner. A passing check does not establish NVIDIA compatibility, performance,
or safe coexistence with other GPU users. Releases still need those checks
against their exact image digests on a trusted NVIDIA host.

The separate [manual release workflow](releases.md) uses a trusted NVIDIA
runner and temporary package publishing authority. It is disabled pending
runner, environment, registry, and release-material verification. Pull-request
jobs cannot select that runner or publish images. Obsolete Gitea workflows
are not retained in the current tree; production deployment remains downstream.

For local source checks, use the [contributor commands](../CONTRIBUTING.md#development-setup).
The new standalone `scripts/smoke-blender-cpu <image>` requires Linux, Docker,
Python 3, and an already built Blender image. It creates temporary containers
and exercises the existing integration modes, including busy shutdown and
overdue-command recovery. The [paired smoke](../scripts/smoke-release-pair)
additionally needs both images and the external `printable-smoke` executable.
