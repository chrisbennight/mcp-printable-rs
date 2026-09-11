# Contributing

Start with a reproducible problem or a concrete use case. For a bug, include the
commit or image pair, Linux and GPU details, relevant configuration names, a
small model or command sequence, and expected versus observed behavior. Remove
credentials and private paths. Use the [security process](SECURITY.md) for
vulnerabilities rather than posting exploit details in an issue.

For a substantial feature, open an issue before implementing it so scope and
acceptance criteria can be agreed. Keep each pull request focused on one
coherent change. Describe the problem, resulting behavior, and validation. A
test result is useful when it proves a user-visible contract; a test that only
repeats implementation details is not.

## Development setup

Install Rust through rustup and use the checked-in `rust-toolchain.toml`.
Native builds need a C++ compiler and CMake. Python 3 runs the bridge unit tests
and repository tooling. Docker is needed for container integration. Ordinary
unit tests use fakes and need neither Blender nor a GPU.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
PYTHONPATH=addon python3 -m unittest discover -s addon/tests -v
PYTHONPATH=scripts python3 -m unittest discover -s scripts/tests -v
python3 scripts/docgate
```

Keep the lockfile with dependency changes. Explain why a new dependency fits,
and check its source and license. Preserve path confinement, bounded responses,
immutable job inputs, and the distinction between completed work and an unknown
mutation outcome. Never interpolate caller data into a shell or interpreter.
Explicit `blender_execute` source is an intentional capability; other inputs
are data and must remain data.

The [architecture guide](docs/architecture.md) describes module boundaries.
Update documentation and examples alongside changed tool behavior. Use
workspace-relative artifact paths and explain units, defaults, and limits.
New capabilities should work through public tools rather than depending on a
particular gateway or model provider.

## Review and support

Open pull requests against `main`. Required GitHub checks must pass; GPU
qualification is a separate trusted release gate. Contributors do not need
access to the maintainer's network, private secret store, or review service.
Maintainers may use automated review, but remain responsible for decisions and
the resulting code. See [CI](docs/continuous-integration.md) for the trust split.

AI-assisted contributions are welcome on the same terms as other contributions.
The author must understand the change, review generated text and dependencies,
run relevant checks, and answer review questions. Include meaningful evidence
and concise explanations; do not submit generated claims that were not verified.

Project support is through GitHub issues, with no guaranteed response time or
service-level agreement. Keep discussion specific and respectful. Explain
technical disagreements with evidence and avoid personal attacks. Harassment,
disclosure of private information, and discriminatory abuse are unacceptable;
maintainers may remove such content or restrict participation. For private
conduct concerns, use GitHub's reporting facilities rather than publishing
someone else's personal information.
