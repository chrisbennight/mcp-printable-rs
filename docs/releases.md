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
signs provenance and an SPDX 2.3 SBOM for each final registry digest, then
publishes the image-set record with their registry digests and evidence. The `release-image`
Actions artifact contains the record's immutable reference. Failed publication
can leave individual images in the registry; deployments consume the completed
record, not an arbitrary component tag.

The record also has a `main` tag for update discovery. It advances only after
the completed record and all component signatures verify for the expected
workflow, source revision and `main` ref. Deployment repositories must pin a
digest, such as `main@sha256:...`; moving the discovery tag does not update an
installed digest. Rerunning an older commit does not move this tag backwards.

Publication uses the workflow's built-in `GITHUB_TOKEN` with `packages: write`
and `attestations: write`, plus short-lived OIDC signing with `id-token: write`.
Test jobs have read-only repository permissions. Public package access is a
registry setting; preserve it when adding packages.

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

## Signed evidence and retention

Each completed record contains `/evidence/index.json`, the exact component
references and tested image IDs, per-image SPDX and Syft inventories, Grype
reports, the policy and vendor fixes used, the KEV identifiers used for evaluation,
installed Debian and Python source and license information, notices, build inputs,
and component signature bundles. The index hashes every retained file. The record's
own provenance signature and an attestation of its evidence index hash are attached
to its immutable OCI digest; they are outside the record to avoid a circular digest.
The index attestation binds independently retained files to that signed record.
Component provenance and SBOM signatures
are also attached to their OCI digests and recorded by GitHub.

Successful evidence is a layer of the existing release-record image, so it remains
available independently of the short-lived Actions artifacts. Preserve the
`sha-<commit>` record tags, component images and signature attachments for as long
as those releases are distributed or supported. Registry deletion still removes
registry evidence; keep an independent copy when long-term availability matters.
Candidate evidence is retained for 14 days in Actions, including failures; scan
reports are separately retained for two days. Failed candidates do not advance
`main`.

Cargo builds use the checksum-verified cargo-auditable 0.7.7 binary. Each distributed
Rust binary retains its dependency graph after stripping. The collector matches
the runtime graph to Cargo's source and license metadata and the locked crate
checksums. `Cargo.lock` is retained as a build input, rather than treated as an
installed-package inventory. The actual patched Manifold and Clipper2 sources
and native build options are retained separately. Stable Cargo metadata can
overestimate dependencies, including packages selected together in a workspace
build and build-time procedural macros; this is
dependency metadata, not a measurement of every linked symbol.

Syft 1.54.1 scans the final squashed image. Removed packages and repository tooling
are excluded. The collector independently reads installed Debian status and Python
distribution metadata and requires those packages to appear in the SBOM. Debian
binary and source versions, including distribution patches, have exact Debian
Snapshot locators. Python source distributions have registry-provided checksums;
wheel-only native distributions use reviewed, version-specific upstream commits
and retained source archives. A changed wheel-only version requires a new reviewed
locator. Native libraries inside wheels retain installed distribution ownership,
exact file hashes and source records. Blender's bundled native libraries and
Python extensions without distribution metadata have individual identities,
available upstream versions, licenses and checksum-bound source locators from
its dependency build specifications. Reviewed file hashes bind each identity to
the pinned binary archive. Orca's copied libraries are matched to the exact Ubuntu
binary packages; their retained source descriptors identify the matching upstream
source and distribution patches. SPDX relationships connect the components and
files to their distribution owner. Source archives, notices and dependency build
specifications remain available alongside the inventory; scanner output alone
cannot recover every statically linked upstream component or determine its legal
obligations.

The SPDX documents and materials also record Blender's upstream dependency
declarations and Orca's external dependency specifications and vendored source
trees as declared build material. These records retain available versions,
licenses, source checksums, source notices and build references, with
`BUILD_DEPENDENCY_OF` relationships. They cover available source material for
dependencies that may be embedded in the application binaries. They do not assert
that every optional or platform-specific upstream declaration is linked into the
Linux binaries. An upstream version absent from vendored metadata stays absent;
the exact containing source archive and subtree identify that material. The
collector distinguishes these declarations from components verified against
installed native files. Full binary linkage and a legal license assessment remain
outside what this evidence can prove.

Orca's wxWidgets build specification names a mutable branch. Its declared source
record preserves that reference and an immutable commit observed during inventory
review, together with the limit that the original compiled commit is unconfirmed.
Printable's provenance authenticates the pinned runtime artifact and this
workflow's source revision; it does not authenticate unavailable upstream build
history.

The collector explicitly adds the downloaded Blender, OrcaSlicer, VirtualGL and
OpenCV bundles, the face landmarker and multiclass selfie segmenter models and
embedded Fira font, with source
locators and applicable notices. It hashes the whole regular-file filesystem and
records links separately. Every native executable and library must have a Debian,
Python, first-party or individually identified native component owner. The Blender
bridge, supervisor and launcher have commit-bound source and MIT license records;
their installed files must match the checked-out sources and retained license.
Missing source locators, required
notices, binary metadata, package coverage or asset hashes stop publication.
An approved bundle directory never identifies an added library. Updating a bundle
requires reviewing its actual native files, source build specifications, available
versions and notices in `release/components.json`; hashes identify distributed
bytes and do not replace upstream versions. Unknown native files require
investigation and an explicit inventory owner;
do not suppress the check or describe unidentified components as fully identified.
These checks preserve distribution evidence and make remaining scanner limits
visible. They do not certify license compliance or byte-for-byte reproducibility.

## Independent verification

Install Docker and GitHub CLI 2.102.0 or newer. Obtain the expected full source
commit independently from the maintained repository. Use the immutable record
reference from the `release-image` artifact or a trusted deployment change:

```sh
record='ghcr.io/chrisbennight/mcp-printable-release:sha-<12-character-commit>@sha256:<digest>'
revision='<full-40-character-commit>'
python3 scripts/verify_release_evidence.py "$record" "$revision" --retain release-evidence
```

The helper verifies the record's OCI signatures before reading its payload, then
checks the evidence hashes, tested-image handoff, retained scan policy and all
five provenance and SBOM signature bundles. It requires the maintained source
repository, `.github/workflows/ci.yml`, `refs/heads/main`, the expected source
commit and a GitHub-hosted runner. It also compares each retained SPDX document
to its verified signed predicate. It never starts the image containers.
An inventory, tag or self-declared source field is not a substitute for those
signature and identity checks.

For independent storage, also save the record's signature bundle and obtain
Sigstore roots through a separately trusted GitHub CLI installation:

```sh
gh attestation download "oci://$record" --repo chrisbennight/mcp-printable-rs
gh attestation trusted-root > trusted_root.jsonl
```

Keep the downloaded record bundle with the retained evidence directory and pin
the trusted-root file through your own trust policy. Do not automatically trust
a root supplied by an unverified release. Verification of retained evidence uses
the saved bundles and does not depend on Actions artifact retention:

```sh
python3 scripts/verify_release_evidence.py "$record" "$revision" \
  --evidence-directory release-evidence \
  --record-bundle '<downloaded-record-bundle.jsonl>' \
  --trusted-root trusted_root.jsonl
```

The images' registry digests and the record digest remain the installation pins.
Successful signature verification authenticates release evidence; it does not
replace runtime tests, GPU validation or the installation's own security policy.

## Local development

`./build-docker.sh` builds and smoke-tests the server without publishing.
`PRINTABLE_SMOKE_PORT` selects its local test port. A local build may set
`CRATES_INDEX_URL` to a reachable mirror; GitHub builds use public crates.io.
The old local publication flags are no longer supported.

Validate workflows with the pinned actionlint used by CI and run the
[contributor checks](../CONTRIBUTING.md#development-setup) before opening
a pull request.
