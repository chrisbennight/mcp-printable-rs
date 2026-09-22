# Build and qualify a matching image set

The [reusable release workflow](../.github/workflows/release.yml) separates
building from GPU execution. A private repository calls it from a manual
`workflow_dispatch` workflow on `main`, pinning the reusable workflow to a
reviewed commit. The public source repository does not register a GPU runner.
Public pull requests continue to use hosted runners without release credentials.

## Execution and authentication

The build job uses `ubuntu-latest` to run source checks, build and publish the
four immutable candidate images, and perform security, native installation,
recovery, and software-rendering tests. It uploads the candidate's source
revision and exact image digests as an immutable Actions artifact.

The GPU job uses the private caller's dedicated runner. It downloads that same
candidate, checks out the same source revision, and runs NVIDIA qualification
against the exact Blender digest. It has read-only repository permission and
no package publishing credential. Its result identifies the complete candidate
by SHA-256. This result is workflow evidence, not a standalone signature:
only artifacts from the current trusted run are eligible for publication.

The final job returns to `ubuntu-latest` and depends on successful build and
GPU jobs. It validates the matching GPU result before publishing the compatible
release record. Both publishing jobs use their automatically issued
`GITHUB_TOKEN` with `contents: read` and `packages: write`. No personal package
publishing token is required. The caller must permit those job permissions and
have publishing access to the destination packages.

The reusable workflow takes these required inputs:

| Input | Meaning |
| --- | --- |
| `source-revision` | Full reviewed source commit reachable from this repository's `main` |
| `crates-index-url` | Approved credential-free Cargo proxy reachable from the hosted runner and its Docker builds |
| `gpu-runner-label` | Dedicated label registered only with the private caller repository |

The GPU runner needs Linux/amd64, Python, NVIDIA drivers and Container Toolkit,
rootless Docker with CDI, and a pre-existing compute workload for coexistence
measurement. `PRINTABLE_GPU_RUNTIME=cdi` selects native CDI device injection;
the standalone smoke defaults to Docker's `--gpus` interface. The smoke resolves
Blender's host PID by its container cgroup, including rootless PID namespaces.
Do not give this runner the production Docker socket or register it with a
public repository. Serialize GPU qualification jobs on the physical host.

Publishing still requires the approved crate proxy. Ordinary source builds and
pull-request CI can resolve public upstreams. Never put a credential-bearing
URL into a build argument.

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
repository URL. The same proxy, security, image identity, and GPU gates apply.

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
