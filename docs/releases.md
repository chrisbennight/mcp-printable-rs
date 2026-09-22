# Build and qualify a matching image set

The [Release workflow](../.github/workflows/release.yml) runs directly in this
public repository. It builds and tests immutable candidates on GitHub-hosted
runners. GPU qualification runs separately in a private repository; this public
repository never registers a persistent GPU runner. Public pull requests use
hosted runners without release credentials.

## Execution and authentication

1. Dispatch **Release** on `main`. The job runs source checks, builds and pushes
   the four candidate images, and performs security, native installation,
   recovery, and software-rendering tests. Only success produces the immutable
   `printable-candidate` Actions artifact with the source revision and digests.
2. A trusted maintainer integration downloads that artifact from the successful
   build and dispatches the private GPU workflow with its exact manifest and
   source revision. That workflow checks main-branch ancestry and uses rootless
   Docker to qualify the exact Blender digest. It has read-only repository
   permission and no package publishing credential.
3. The integration verifies the private workflow's successful conclusion,
   reviewed workflow revision, runner identity, and downloaded proof. The proof
   must contain `gpu: passed` and the SHA-256 of the complete candidate, using
   `release_candidate.digest`. Only then does the integration post a successful
   commit status on the candidate source revision. Its context is
   `printable/gpu/<public-build-run-id>` and its description is
   `sha256:<candidate-hash>`. Retain the private proof and run identity for audit;
   do not include private host or repository details in the public status.
4. Dispatch [Publish qualified release](../.github/workflows/publish-release.yml)
   on `main` with `candidate-run-id` set to that public build run. The job checks
   the build's repository, workflow, event, branch, source revision, and success,
   then downloads its candidate artifact. It requires the latest matching
   status to be successful, posted by the configured trusted account, and bound
   to the exact candidate hash before publishing the release record. A later
   failure status revokes an earlier success. A proof file or status from an
   arbitrary account is insufficient.

Both hosted workflows belong to the public repository, including their usage
accounting. Their publishing jobs use the automatically issued `GITHUB_TOKEN`
with `packages: write`. No personal publishing token is required. The trusted
integration's status-writing authority stays outside these jobs; it is the
bridge from private qualification evidence to public publication and must not
approve an unverified result. This is a maintainer-operated sequence, not an
automatic cross-repository trigger.

Configure these repository settings before dispatch:

| Setting | Meaning |
| --- | --- |
| Variable `PRINTABLE_QUALIFIER_ID` | Numeric GitHub account ID of the trusted integration that verifies private GPU evidence and posts approval statuses |

The GPU runner needs Linux/amd64, Python, NVIDIA drivers and Container Toolkit,
rootless Docker with CDI, and a pre-existing compute workload for coexistence
measurement. `PRINTABLE_GPU_RUNTIME=cdi` selects native CDI device injection;
the standalone smoke defaults to Docker's `--gpus` interface. The smoke resolves
Blender's host PID by its container cgroup, including rootless PID namespaces.
Do not give this runner the production Docker socket or register it with a
public repository. Serialize GPU qualification jobs on the physical host.

Hosted releases and pull-request CI resolve crates.io using the checked-in
lockfile. A local build or publication can set `CRATES_INDEX_URL` to a reachable
mirror; that setting applies to both Docker builds and host Cargo commands.
Never put a credential-bearing URL into a build argument.

Images use `ghcr.io/chrisbennight/mcp-printable-rs`,
`ghcr.io/chrisbennight/mcp-printable-blender`,
`ghcr.io/chrisbennight/mcp-printable-cad`,
`ghcr.io/chrisbennight/mcp-printable-slicer`, and
`ghcr.io/chrisbennight/mcp-printable-release`. New GHCR packages default to
private visibility. Configure these packages for public distribution and
verify anonymous pulls of the exact digests before deployment. Publishing
permission and public download access are separate settings. See
[GitHub's container registry guidance](https://docs.github.com/en/packages/working-with-a-github-packages-registry/working-with-the-container-registry).

## Qualification and consumption

The publisher runs source tests before publishing commit-scoped server,
Blender, CAD, and slicer images, obtains their registry digests, and inspects the immutable
images for the expected source revision, role, architecture, runtime settings,
and embedded credentials. The existing vulnerability policy rejects applicable
fixed high/critical vulnerabilities and known-exploited vulnerabilities.
Container and paired-render tests run against those digests. The CAD worker
also runs native geometry, STEP import, and export tests in a restricted
container based on its exact digest. The slicer runs native preparation, toolpath
review and restart recovery against its packaged Orca release. The installation
test also verifies CAD-to-slice requests and downloaded hashes through public MCP;
packaged first-party and font notices are checked. NVIDIA tests
require EGL, Eevee, OptiX/Cycles activity, a visible failure without the required
GPU, and continued presence of the existing GPU workload on the selected device.

Only after those checks pass does the script publish a record identifying the
compatible image set, including the CAD image and digest in
`org.printable.cad.image` and `org.printable.cad.digest`, and the slicer in
`org.printable.slicer.image` and `org.printable.slicer.digest`. The existing server
and Blender labels remain unchanged. A failed qualification can leave candidate
images in the registry; their existence is not release approval. Consume the
verified server, Blender, CAD, and slicer digest references together. Keep the previous set and a
workspace backup for rollback; image rollback does not reverse saved data.
There is no mutable production channel or automatic deployment in this flow.

For a standalone release or another registry, an authenticated maintainer can run
`./build-docker.sh --push` on an equivalent trusted host with
`PRINTABLE_REGISTRY`, `PRINTABLE_IMAGE_NAMESPACE`, and
`PRINTABLE_SOURCE_REPOSITORY` set. The source must be a credential-free HTTPS
repository URL. The same security, image identity, and GPU gates apply.

`./build-docker.sh --push-candidate` runs all source and CPU qualification but
stops before GPU qualification and release-record publication. It writes
`target/release/candidate.json` only after those checks pass. The workflow
transfers this file without rebuilding the images on the GPU machine.

`build-docker.sh` binds its isolated MCP smoke container to loopback port 8000.
Set `PRINTABLE_SMOKE_PORT` to another available TCP port when that port is
already in use. Both local builds and `--push` qualification use this setting;
it does not change the service's installation port.

No GitHub image pair has been qualified by this migration yet. Public binary
distribution also needs the exact dependency inventory, corresponding source
and build materials for copyleft components, and retained qualification
evidence associated with the pair. A successful private build or an SBOM alone
does not establish that those distribution obligations are complete.

## Local policy checks

```sh
install -d -m 0700 .dev
python3 -m venv .dev/tooling
.dev/tooling/bin/python -m pip install -r requirements-tooling.txt
PYTHONPATH=scripts .dev/tooling/bin/python -m unittest discover -s scripts/tests -v
PATH="$PWD/.dev/tooling/bin:$PATH" sh scripts/check-release-target
```

The workflow policy uses PyYAML's non-executing scalar loader to inspect the
GitHub configuration. Mutation tests reject release authority in untrusted CI,
unpinned actions, missing publication gates, and mutable image publication.
