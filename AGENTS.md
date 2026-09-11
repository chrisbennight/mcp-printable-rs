# AGENTS.md

Guidance for working in `mcp-printable-rs`.

## GitHub migration

The maintained source is now `https://github.com/chrisbennight/mcp-printable-rs`,
initially private. GitHub history begins with a source snapshot; earlier history
remains on Gitea. Use GitHub MCP for issues, pull requests, and Actions, and local
Git for checkouts, commits, and pushes. Create worktrees from the freshly fetched
GitHub `main` branch. Keep changes on branches and preserve the user's checkout.

The [GitHub CI workflow](.github/workflows/ci.yml) is the active verification
path. The private release and deployment instructions below describe the
imported pipeline pending its separate port; they do not authorize connecting
GitHub pull requests to lab runners, secrets, or production deployment. See
[continuous integration](docs/continuous-integration.md).

The supported deployment is self-hosted Linux/amd64 with NVIDIA. CPU/software
checks remain useful for CI but do not establish a non-NVIDIA support promise.
Repository visibility stays private until explicitly authorized otherwise.

Independent installations use [Compose and direct HTTP clients](docs/installation.md).
The gateway-only caller and secret-provider rules below describe the imported
lab deployment, not a restriction on the independent product. The server also
supports mounted bearer files and an explicit HTTPS download base; `.env.example`
is the configuration reference.

## Overview

Production Printable MCP service for AI-driven 3D modeling, rendering,
animation, and FDM print validation through headless Blender and OpenSCAD.
Exposes MCP tools over streamable HTTP at `/mcp`; stdio is not supported.

- Maintained repository: `https://github.com/chrisbennight/mcp-printable-rs`
- Python is retained only for first-party code that runs inside Blender via
  `bpy`. The authoritative add-on and headless launcher live in `addon/`.
- The target deployment is Linux/amd64 `server`: a Rust MCP container beside a
  persistent headless Blender 5.2.0 container using the RTX 4060 Ti
  non-exclusively. Deploy and gateway wiring live in `../docker-home`; the
  Blender image source lands here with its delivery slice.

## Plan of record

[`PLAN.md`](PLAN.md) is the approved capability-delivery plan: architecture,
ordering, and product exit criteria. Running decisions accumulate in
[`DECISIONS.md`](DECISIONS.md). Read both before starting any slice.

## Product authority

User-facing capability, reliability, performance, and security define
correctness. Test those contracts directly. Preserve a name or wire shape only
when it protects an active workflow or is deliberately retained as a stable
product surface.

## Architecture

Cargo workspace, one crate per concern:

- `crates/printable-server` — binary: streamable HTTP at `/mcp`, `/healthz`,
  hand-rolled `ServerHandler` (house style), tool dispatch, resources.
- `crates/printable-blender` — TCP client for the Blender addon bridge
  (length-prefixed JSON, deadlines, version handshake, FakeAddon test harness).
- `crates/printable-geom` — pure mesh geometry/printability (parry3d +
  Manifold); no I/O, no async; property- and mutation-tested.
- `crates/printable-scad` — OpenSCAD subprocess backend + confined-source gate.
- `crates/printable-workspace` — capability-rooted confined artifact I/O.
- `crates/printable-imaging` — render compositing with a bundled font.
- `addon/` — first-party Blender add-on, pure bridge support, and headless
  main-thread launcher.
- `blender/` — checksum-pinned headless Blender image; release promotion is
  gated on the exact published digest passing the production GPU smoke.

## Commands

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
python3 scripts/docgate
```

CI and release artifacts target Linux/amd64 on `ubuntu-latest`, matching the
production `server` host. A PR is not done until every required context
configured for that PR is green.

## Code style

- Rust 2024 edition; `cargo fmt` is the formatter; `cargo clippy -- -D
  warnings` is the lint gate — no allow-by-default.
- `thiserror` for library-level errors (with a machine-readable `.code()`),
  `anyhow` only at the binary boundary.
- `tracing` for logs; never `println!` outside the bin entry point/CLI help.
- Default to no comments; names should be self-explanatory. A comment states a
  constraint the code cannot show.
- Untrusted input (OpenSCAD source, caller paths) never reaches a shell or
  escapes the workspace root — argv arrays and capability-rooted I/O only.

## Configuration

`.env.example` is the canonical list for implemented Rust settings. Core names
are `PRINTABLE_HTTP_HOST`, `PRINTABLE_HTTP_PORT`, `BLENDER_HOST`,
`BLENDER_PORT`, `PRINTABLE_WORKSPACE_ROOT`,
`PRINTABLE_BLENDER_WORKSPACE_ROOT`, `OPENSCAD_BIN`,
`PRINTABLE_SCAD_CONCURRENCY`, `FFMPEG_BIN`,
`PRINTABLE_RENDER_JOB_QUEUE_DEPTH`, `PRINTABLE_GEOMETRY_WORKER_BIN`,
`PRINTABLE_GEOMETRY_WORKER_MEMORY_MIB`, `PRINTABLE_MCP_BEARER`,
`PRINTABLE_ALLOWED_HOSTS`, and `PRINTABLE_ALLOWED_ORIGINS`. Container deployments must allow the actual
gateway Host header forms.

## Homelab infrastructure

- **Secrets**: Infisical only, fetched at workflow runtime
  (`infisical-secrets-action`); never commit credentials or bearer values.
- **Deployment**: build here, deploy in `../docker-home` as a Komodo stack on
  `server`. Printable validates the one shared Infisical-backed bearer on
  `/mcp`; the gateway is the only supported remote caller.
- **CI runners**: `ubuntu-latest` fleet (amd64, DinD, on the server host).

## Image and release flow

`.gitea/workflows/build.yml` releases the Rust and Blender images. Pull requests
run workspace tests, build both amd64 images, and exercise their container
smokes. Main publishes matching commit-scoped image tags plus a release-pair
record containing both immutable registry digests before Komodo deployment;
tags are discovery pointers, not the compatibility contract.
`build-docker.sh` preserves test-before-publish,
CPU-and-production-GPU-smoke-before-pair ordering; publishing is opt-in
(`--push`) and runs on `server`.

Crates reach every build through whatever `CRATES_INDEX_URL` names. Cargo reads
no environment variable for a mirror, so that address becomes a source
replacement three ways, one per execution context: a `.cargo/config.toml`
written inside the image for the Rust Dockerfile, the same file written in the
checkout by each workflow job that compiles on the runner, and cargo's own
`--config` assignments for the publishing script's host-side commands — which
must not write a file, because that path refuses to publish from a dirty
worktree. A job's file cannot carry to another job; each one writes its own.

Nothing is committed: an address that resolves only on one network would stop
these images building anywhere else. Unset, cargo resolves from crates.io;
`--push` and the publishing workflow refuse instead, because an image that
skipped the proxy skipped its cache, audit, and blocklist.

- The runtime image is `debian:trixie-slim` (not distroless — OpenSCAD, Xvfb,
  and FFmpeg need apt); OpenSCAD renders under a virtual framebuffer via
  `scripts/openscad-headless`, and the image pins both subprocess paths.
- The container smoke is driven by the `printable-smoke` binary, which speaks the
  MCP wire protocol from outside the application container — it is never shipped
  in the runtime image. `smoke/expected-tools.txt` is the release catalog: the
  smoke compares the exact advertised names against it, executes the packaged
  geometry and OpenSCAD workflows, verifies the packaged FFmpeg encoder, and
  later releases update that file alongside the server catalog.
- Rust and Blender production images are Linux/amd64 and receive real NVIDIA
  validation on `server`, not a pretend GPU CI result.
- Registry and Komodo material come from the repo-scoped Infisical path
  `/bennight/mcp-printable-rs` at workflow runtime. Production compose/stack files
  belong in `../docker-home`, not this repo.

## Git workflow

- Every task starts in a worktree created from a freshly fetched
  `origin/main`. Do not edit the primary checkout.
- Ask before committing, pushing, or opening a PR unless the user directly
  invoked `/pr-and-monitor`; that invocation authorizes the complete loop.
- Never push directly to `main`. Stage specific paths, never `git add .`.
- Use `tea` for Gitea operations (never `gh` against Gitea). Treat PR
  descriptions as immutable; post corrections as comments.
- Merge only when required CI statuses are green and AERB has no unresolved
  findings.

## Boundaries

Do not:

- commit secrets, or add production compose files (those belong in
  `docker-home`);
- make live calls to Blender, OpenSCAD binaries, or the network from unit tests
  — use fakes there; dedicated container integration and server GPU smokes are
  the sanctioned live boundaries;
- give Printable a Docker socket, privileged mode, host PID namespace, broad
  NAS mount, public Blender port, or control path to unrelated workloads.

## Safety

Ask before commit/push/PR (see Git workflow). Treat destructive operations —
force-push, branch deletion, registry tag changes, prunes — as requiring
explicit say-so. Never paste secret material into commits, PR bodies, or chat.
