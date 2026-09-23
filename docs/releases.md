# Releases

The [CI workflow](../.github/workflows/ci.yml) runs on pull requests and pushes
to `main`. All jobs run on GitHub-hosted Linux/amd64 runners without a GPU.

## Publication

After a push to `main`, successful source, workflow, fuzz and container checks
allow the publication job to run in the same workflow. Pull requests cannot
publish. No manual dispatch, private runner, approval status or external
release coordinator is required.

The container job saves the images it tested. The publication job loads those
images and checks their IDs, source revision and runtime configuration. It scans
them under the existing vulnerability policy before pushing anything. It does
not rebuild the runtime images.

Server, Blender, CAD and slicer images receive `sha-<commit>` tags in GHCR.
The publisher checks that downloaded images match the tested image IDs, then
publishes the image-set record with their registry digests. The `release-image`
Actions artifact contains the record's immutable reference. Failed publication
can leave individual images in the registry; deployments consume the completed
record, not an arbitrary component tag.

The record also has a `main` tag for update discovery. It advances only after
publication succeeds for current `main`. Deployment repositories must pin a
digest, such as `main@sha256:...`; moving the discovery tag does not update an
installed digest. Rerunning an older commit does not move this tag backwards.

Publication uses the workflow's built-in `GITHUB_TOKEN` with `packages: write`.
Test jobs have read-only repository permissions. Public package access is a
registry setting; preserve it when adding packages.

## Installation and updates

The record identifies a matching server, Blender, CAD and slicer set.
Installations pin its digest and resolve the component references from its
labels. A downstream deployment repository reviews image updates through its
own pull requests and deploys after merge. No private deployment configuration
or credentials belong in this repository.

CI checks software rendering, native CAD and slicing, installation, recovery,
notices and image security. It does not measure NVIDIA performance or driver
compatibility. `scripts/smoke-blender-gpu` remains an optional installation
diagnostic, not a release requirement.

Keep release source, dependency inventories, notices and corresponding source
available as described in [the license guidance](../THIRD_PARTY_NOTICES.md).
See [installation](installation.md) for runtime configuration and rollback.

## Local development

`./build-docker.sh` builds and smoke-tests the server without publishing.
`PRINTABLE_SMOKE_PORT` selects its local test port. A local build may set
`CRATES_INDEX_URL` to a reachable mirror; GitHub builds use public crates.io.
The old local publication flags are no longer supported.

Validate workflow permissions with `python3 scripts/workflow_policy.py` and
run the [contributor checks](../CONTRIBUTING.md#development-setup) before opening
a pull request.
