# Blender GPU smoke — 2026-07-18

This is historical lab evidence, not qualification of a GitHub release pair.

The controlled smoke passed on production host `server` against the
Linux/amd64 candidate image built from this change.

| Field | Observed value |
|---|---|
| Image content ID | `sha256:87ec40045555e8db1f21e65e3670a237388dc5905d1f1793dcea64dc002cd1f6` |
| Image user | `10001:10001` |
| GPU | NVIDIA GeForce RTX 4060 Ti |
| Driver | `580.159.03` |
| EEVEE graphics backend | OpenGL, NVIDIA, `NVIDIA Corporation`, version `4.6.0 NVIDIA 580.159.03` |
| Cycles device | OptiX, NVIDIA GeForce RTX 4060 Ti |
| Blender GPU memory before Cycles | 552 MiB |
| Peak Blender GPU memory during Cycles | 1430 MiB |
| Peak Blender SM utilization during Cycles | 73% |
| Cycles observation samples | 50 |
| Pre-existing peer GPU PID | `14103` |
| `DISPLAY` | unset |
| Published host ports | none |
| Required GPU unavailable | startup failed visibly |

The smoke rendered non-empty 512×512 EEVEE and 1024×1024 Cycles/OptiX PNGs,
observed Blender memory rise from 552 MiB to 1430 MiB with 73% peak SM use
during 50 Cycles-only samples, and confirmed that the same pre-existing peer GPU
process was present before and after each stage and at every Cycles sample. The
script did not signal, stop, or inspect the peer's owning container.

The candidate tag was local to the controlled host; the content ID identifies
the exact tested local image. The release workflow publishes the reviewed
source under matching commit-scoped Rust and Blender registry tags, resolves
their immutable registry digests, repeats the GPU smoke against the exact
Blender digest, and then publishes a pair record containing both digests.

Re-run from the repository checkout on `server` with:

```sh
scripts/smoke-blender-gpu \
  gitea.cacahuate.org/bennight/mcp-printable-blender:sha-<commit>@sha256:<digest>
```
