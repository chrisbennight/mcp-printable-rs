# Build and qualify an image pair

The [release workflow](../.github/workflows/release.yml) is manual, restricted
to `main` in the private repository, and disabled unless the repository variable
`PRINTABLE_RELEASE_ENABLED` is `true`. Adding this workflow does not register a
runner, configure environment protection, publish an image, or deploy a service.

## Runner and registry prerequisites

Use a trusted, dedicated GitHub runner labelled `self-hosted`, `Linux`, `X64`,
and `printable-release`. It needs Docker Engine with Buildx, Python 3.11 or
newer with venv support, the repository's Rust toolchain, CMake, a C++ compiler,
curl, and the NVIDIA Container Toolkit. A pre-existing compute workload must be
running on the selected GPU throughout qualification; the test does not stop it.
Do not expose this runner to pull requests or other untrusted repositories.

Before enabling the workflow, restrict the `printable-release` environment to
the protected `main` branch and configure maintainer approval where supported
by the account's GitHub plan. Verify runner access restrictions and package
permissions. The workflow uses GitHub's temporary token with package-write
permission and a temporary Docker credential directory; it does not need a
stored registry password, Infisical, SSH, or Komodo.

The repository variables are:

| Variable | Meaning |
| --- | --- |
| `PRINTABLE_RELEASE_ENABLED` | Explicit opt-in to run the publisher; leave unset until prerequisites are verified |
| `CRATES_INDEX_URL` | Approved credential-free Cargo proxy URL reachable from both the host and Docker builds |
| `PRINTABLE_GPU_DEVICE` | One NVIDIA device index or GPU UUID; defaults to `0` |

Publishing still requires the approved crate proxy. Ordinary source builds and
pull-request CI can resolve public upstreams. Making the proxy optional for
publishing is a separate policy decision; this port preserves that safeguard.
Never put a credential-bearing URL into a build argument.

Images default to `ghcr.io/chrisbennight/mcp-printable-rs`,
`ghcr.io/chrisbennight/mcp-printable-blender`, and
`ghcr.io/chrisbennight/mcp-printable-release`. The workflow derives the namespace
from the repository owner. GitHub documents that newly published container
packages are private by default; an existing public package remains a separate
visibility decision. Verify all destination packages are private before the
first dispatch. See [GitHub's container registry guidance](https://docs.github.com/en/packages/working-with-a-github-packages-registry/working-with-the-container-registry).

## Qualification and consumption

The publisher runs source tests before publishing commit-scoped server and
Blender images, obtains their registry digests, and inspects the immutable
images for the expected source revision, role, architecture, runtime settings,
and embedded credentials. The existing vulnerability policy rejects applicable
fixed high/critical vulnerabilities and known-exploited vulnerabilities.
Container and paired-render tests run against those digests. NVIDIA tests
require EGL, Eevee, OptiX/Cycles activity, a visible failure without the required
GPU, and continued presence of the existing GPU workload on the selected device.

Only after those checks pass does the script publish a record identifying the
compatible pair. A failed qualification can leave candidate images in the
registry; their existence is not release approval. Consume a verified pair's
server and Blender digest references together. Keep the previous pair and a
workspace backup for rollback; image rollback does not reverse saved data.
There is no mutable production channel or automatic deployment in this flow.

For another private registry, an authenticated maintainer can run
`./build-docker.sh --push` on an equivalent trusted host with
`PRINTABLE_REGISTRY`, `PRINTABLE_IMAGE_NAMESPACE`, and
`PRINTABLE_SOURCE_REPOSITORY` set. The source must be a credential-free HTTPS
repository URL. The same proxy, security, image identity, and GPU gates apply.

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
