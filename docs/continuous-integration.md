# Continuous integration

The [CI workflow](../.github/workflows/ci.yml) runs on pull requests and pushes
to `main`. Manual CI runs are available for diagnostics but do not publish.
All jobs use GitHub-hosted Linux/amd64 runners; no GPU or private network is needed.

Jobs retain their check names and select work from the Git merge-base diff on a
pull request or the push diff on `main`. Rust, addon Python, repository scripts,
documentation, workflow syntax, the confined-source fuzzer, and the matching
image set each use their actual inputs. Documentation-only changes skip Rust,
fuzz tooling and container builds. Rust test-only changes run source tests
without rebuilding runtime images. Manual runs select all checks. Skipped work
does not establish coverage for that component.

- Source checks run Rust formatting, lint and tests, Python tests, documentation
  validation, and shell checks.
- Workflow validation checks GitHub Actions syntax and publication permissions.
- Fuzz smoke exercises the confined OpenSCAD source gate when its source,
  dependency graph, fuzz inputs or CI configuration changes.
- Container integration builds the server, Blender, CAD and slicer images and
  exercises native geometry, slicing, notices, installation, recovery, views
  and isolated rendering with software graphics.
- On a main-branch push with image inputs, publication waits for every check,
  scans the tested images, and publishes them without rebuilding. See
  [releases](releases.md).

Pull-request jobs cannot publish images or trigger production deployment.
Deployment belongs to the installation's configuration repository.

The supported runtime can use NVIDIA acceleration. Passing software-rendering
tests does not claim GPU performance or compatibility with every driver.
Optional GPU diagnostics do not block release publication.

For local checks, use the [contributor commands](../CONTRIBUTING.md#development-setup).
`scripts/smoke-blender-cpu <image>` exercises an existing Blender image without
a GPU. `scripts/smoke-release-pair` also needs the server image and the external
`printable-smoke` executable.
