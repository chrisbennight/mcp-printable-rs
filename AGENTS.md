# Working on Printable

The maintained repository is https://github.com/chrisbennight/mcp-printable-rs.
The supported installation is self-hosted Linux/amd64 with NVIDIA. Keep the
repository free of private deployment details. The source repository is public;
package visibility is a separate decision requiring explicit authorization.

Read [CONTRIBUTING.md](CONTRIBUTING.md) and the
[architecture guide](docs/architecture.md). Current work is tracked in GitHub
issues. `PLAN.md` and `DECISIONS.md` retain historical delivery context; private
lab deployment instructions are not prerequisites for contributing.

## Product and code boundaries

User-facing capability, reliability, performance, and security define
correctness. Preserve a wire shape when it protects an active workflow or a
stable product contract. Make the smallest coherent change and record a
disposition for every review finding.

- Rust uses edition 2024, cargo fmt, and clippy with warnings denied.
- Use thiserror for library errors and tracing for application logs. Redact
  sensitive fields; do not commit debug prints or stack traces.
- Keep pure geometry separate from I/O and async code. Exact CSG runs in a
  disposable worker with a bounded address space.
- The Blender add-on and supervisor live in addon/. Python repository scripts
  support builds, verification, and direct-client examples; they do not replace
  the Rust server.
- Treat caller paths and OpenSCAD input as untrusted data. Use argv arrays and
  capability-rooted I/O. Never construct shell commands from caller input.
- Explicit Blender Python is an authorized capability, not a language sandbox.
  Do not give the containers credentials, a Docker socket, privileged mode,
  host PID access, broad host mounts, or a public Blender bridge.
- Preserve immutable checkpoints, one-use file grants, scene preconditions,
  and honest unknown outcomes after a delivered mutation times out.
- Update .env.example for implemented settings. The server reads process
  environment; do not claim a file is automatically loaded.

## Validation

Use the isolated Python tooling environment from the contributor setup before
running these commands. Release-policy tests require `requirements-tooling.txt`.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
PYTHONPATH=addon python3 -m unittest discover -s addon/tests -v
PYTHONPATH=scripts python3 -m unittest discover -s scripts/tests -v
python3 scripts/docgate
```

Unit tests use fakes and must not contact production systems. Dedicated
container integration is the live software test boundary; trusted NVIDIA
qualification is separate. Read an explicit successful exit status before
claiming a gate passed. Reuse valid evidence for an unchanged candidate.

## Git and release workflow

Work in a branch worktree under a gitignored .worktrees/ directory, based on a
freshly fetched GitHub main. Preserve the primary checkout and other people's
changes. Stage specific paths and review the staged diff. Never push directly
to main, rewrite history, publish images, or change visibility without the
applicable authorization. Existing task authorization remains valid; do not
ask again for actions the user has already approved.

A pull request is ready only when required GitHub checks pass and review
findings have dispositions. Maintainer automation uses AERB when available;
contributors do not need access to that private review service. Automated
review does not replace human responsibility for the change.

Server, Blender, and CAD images form a matching set identified by immutable digests and source revision.
Do not promote software-rendered test results as NVIDIA qualification. Keep
pull requests off persistent GPU runners and away from release credentials.
Do not couple the general installation to a private registry, secret provider,
or deployment controller. Site-specific production wiring belongs downstream.

The [manual release workflow](docs/releases.md) is disabled until its trusted
runner, environment protections, private package destinations, and release
materials are verified. Publishing retains the approved crate-proxy gate;
ordinary local source builds do not require that proxy.

Write plain English. Documentation should state current behavior, give runnable
commands, and distinguish measured evidence from assumptions. Keep design
history out of user-visible errors and source comments. Preserve license and
attribution notices for bundled components.
