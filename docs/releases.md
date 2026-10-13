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

The `release-security-scans` Actions artifact retains completed Grype reports,
including when a finding blocks publication. Each report identifies its source
revision, image role and tested image ID, so maintainers can inspect the matched
package and version before choosing a remediation. Scan failures also log the
matched package, version and file locations as bounded JSON records; truncation
is explicit and the retained report contains the full evidence. The vulnerability policy
continues to decide whether publication may proceed.

Scans apply the reviewed [vendor fixes](../release/vendor-fixes.vex.json) in
OpenVEX format. These record vendor-confirmed fixes for exact package versions
when scanner data still reports the vulnerability. They do not waive an
unfixed vulnerability or suppress the same vulnerability in other packages or
versions. Grype retains the corrected matches and their VEX reason under
`ignoredMatches` in each scan report. The severity and known-exploited
vulnerability policy continues to apply to every remaining match.

Server, Blender, CAD, slicer and photo reconstruction images receive
`sha-<commit>` tags in GHCR.
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

## SBOMs and attestations

The container job runs pinned Syft against each tested image ID and retains its
unmodified SPDX 2.3 document. Rust executables carry cargo-auditable dependency
metadata. The [component supplement](../release/components.json) records declared
inputs that scanners can miss, such as bundled native programs, fonts and model
assets. It identifies exact source and recipe locations; it does not establish
binary linkage or exhaustive file ownership. Operator-supplied models and private
configuration are outside the distributed inventory.

GitHub signs standard build-provenance and SBOM attestations for each published
component digest. The publisher verifies their repository, workflow, source
revision and main-branch origin using the official GitHub CLI before completing
the release record. The record carries SBOMs, the supplement, repository notice
copies, scan reports, policy inputs and attestation bundles under `/evidence`.
Its own build provenance protects that payload. Scanner limitations and declared inputs remain
visible; these materials do not claim an exhaustive reconstructed inventory.

For an immutable record or component reference, use the
[GitHub verification guidance](https://cli.github.com/manual/gh_attestation_verify):

```sh
gh attestation verify "oci://$IMAGE_REFERENCE" --bundle-from-oci \
  --repo chrisbennight/mcp-printable-rs \
  --signer-workflow chrisbennight/mcp-printable-rs/.github/workflows/ci.yml \
  --source-ref refs/heads/main --source-digest "$SOURCE_REVISION" \
  --deny-self-hosted-runners
```

For a component's SBOM attestation, add
`--predicate-type https://spdx.dev/Document/v2.3`. Set `IMAGE_REFERENCE` to a
digest-pinned public reference and `SOURCE_REVISION` to its full release commit.
Use the same command for an older supported release. Releases predating this
capability do not acquire attestations retroactively.

To inspect a verified record without starting it, create a stopped container
and copy its files:

```sh
docker create --name printable-release-inspect "$IMAGE_REFERENCE" /unused
docker cp printable-release-inspect:/evidence ./release-evidence
docker rm printable-release-inspect
```

Installed third-party notices remain in the component images identified by the
record. The supplement lists notice locations for known bundles; operating-system
and Python packages also retain their installed documentation and license metadata.
Verify the component's digest-pinned reference with the command above, then copy
the relevant notice using a stopped container. For example, OpenCV's notice is
available from the reconstruction image:

```sh
docker create --name printable-notice-inspect "$COMPONENT_REFERENCE" /unused
docker cp printable-notice-inspect:/opt/opencv-python/LICENSE-OpenCV ./LICENSE-OpenCV
docker rm printable-notice-inspect
```

`COMPONENT_REFERENCE` must be the verified reconstruction reference from the
release record. Extracting `/evidence` alone does not collect every dependency's
notice text.

Keep the immutable record, its component images and associated OCI attestations
available for as long as the release is offered. Retain required corresponding
source separately under [the license guidance](../THIRD_PARTY_NOTICES.md); source
locators and SBOMs do not replace that obligation. Short-lived Actions artifacts
are diagnostic copies, not the supported consumer access path.

## Installation and updates

The record identifies a matching server, Blender, CAD, slicer and photo
reconstruction set. The reconstruction image is the one CI tests: it carries
CPU-only PyTorch and runs without a GPU.
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

Validate workflows with the pinned actionlint used by CI and run the
[contributor checks](../CONTRIBUTING.md#development-setup) before opening
a pull request.
