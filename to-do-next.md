# Next follow-up work

These recommendations follow the architecture review of source revision
`c45474619c659eb0854642e6920829cf737d358e`. The initial work is tracked in
[epic #136](https://github.com/chrisbennight/mcp-printable-rs/issues/136) and its
sub-issues. This file retains broader objectives for later prioritization;
it does not prescribe an implementation or claim that the work is complete.

## Evidence for every improvement

Start with an observed problem or repeatable workflow, its baseline, and written
success criteria. Keep source and image identities, environment, workloads,
budgets, and trial conditions in the evidence. Retain failures as well as passes.
Measure server and worker memory, latency distributions, task correctness,
incorrect or avoidable calls, retries, and recovery steps where relevant.

Record text and image tokens from actual provider usage when available. Mark
unavailable measurements explicitly; JSON bytes, image bytes, and tool counts
are not token estimates. Use equivalent tasks and budgets for comparisons and
choose acceptance thresholds before examining the changed candidate. Keep GPU
qualification distinct from software-rendered tests.

Use public-safe fixtures and isolated fake services or test containers. Keep
credentials, private network layouts, private hostnames, unrelated user models,
and deployment-specific control systems out of documentation and evidence.

## Architecture and contracts

- [ ] **Make ownership and failure boundaries easier to understand.** Identify
  changes that make process lifetime, resource policy, cancellation, and response
  handling clearer to maintainers. Success is a documented ownership boundary
  with proportionate regression evidence and preserved public behavior. File
  length alone does not establish a need to split a module.
- [ ] **Keep machine outcomes independent of human wording.** Some job outcomes
  are classified through message equality or prefixes, and several output
  contracts are written separately from result types. Success is stable machine
  behavior when human messages change and verifiable agreement between results
  and public contracts. Coordinate with
  [structured result correctness #139](https://github.com/chrisbennight/mcp-printable-rs/issues/139).
- [ ] **Make stalled assembly work recoverable within caller budgets.** A blocked
  native analysis can occupy the geometry lane without a caller-controlled end.
  Success is an explicit outcome within the chosen work budget, confirmed process
  cleanup, and continued availability of unrelated capabilities. Preserve large
  model support and honest interruption outcomes. Coordinate with
  [assembly resource containment #138](https://github.com/chrisbennight/mcp-printable-rs/issues/138).
- [ ] **Improve selected action discovery across engines.** Agents should obtain
  the precise input and output contract they need without avoidable full-catalog
  exposure. Success includes correct calls through independently implemented
  clients and measured discovery friction, while retaining ordinary MCP access.
  Reuse [on-demand discovery #20](https://github.com/chrisbennight/mcp-printable-rs/issues/20)
  and [nested printer contracts #30](https://github.com/chrisbennight/mcp-printable-rs/issues/30).

## Releases and installation

- [ ] **Expose release-blocking dependency findings before merge.** A successful
  source and container check can currently be followed by a failed publication
  scan. Success is credential-free pre-merge evidence of the same relevant policy
  failures, with fresh release-time intelligence still considered and publication
  authority kept out of contribution jobs.
- [ ] **Verify ongoing dependency maintenance.** Configuration does not establish
  that update automation runs or that native and bundled components are covered.
  Success is evidence of active coverage, known gaps, and a maintenance owner.
  Coordinate with [dependency update coverage #49](https://github.com/chrisbennight/mcp-printable-rs/issues/49).
- [ ] **Let a new user install a published image set from public instructions.**
  The existing local-build path is more concrete than the published-image path.
  Success is an independent installation using a verified complete image set,
  without manually guessing compatible component tags or needing private
  infrastructure. Use the inventories and attestations from
  [release verification #142](https://github.com/chrisbennight/mcp-printable-rs/issues/142).
- [ ] **Make upgrade decisions understandable.** Each offered image set should
  communicate required operator actions, compatibility and data changes,
  relevant security fixes, known limits, and rollback constraints. Success is
  that an operator can assess an update without reconstructing its implications
  from the entire commit history.

## Operations and testing

- [ ] **Provide useful diagnostics that are safe to share.** Operators need exact
  image identities, engine versions, readiness, storage completeness, and bounded
  relevant job failures. Success is sufficient evidence to diagnose representative
  failures while excluding credentials, raw environment maps, and unrelated
  artifacts. Preserve the distinction between liveness, readiness, and active work.
- [ ] **Make development-test requirements explicit and results dependable.**
  Several Python tests rely on Linux process inspection or resource limits.
  Success is a runnable documented environment, clear platform requirements,
  explicit skipped-test reporting, and meaningful isolated coverage. Resolve the
  known [fork and storage-lease test race #98](https://github.com/chrisbennight/mcp-printable-rs/issues/98)
  using evidence of the actual ownership contract.
- [ ] **Retain representative NVIDIA resource and compatibility evidence.**
  Success is repeatable workload evidence tied to exact images, GPU and driver,
  host resources, and a stated sampling method. Measure concurrent live modeling
  and background work where relevant; do not infer GPU performance from CPU CI.
- [ ] **Reconcile process ownership and inherited authority.** The review raised
  a possible process-group identity race that was not reproduced, and observed
  differing worker environment policies. Success is confirmed cleanup of owned
  processes and only the runtime authority each worker requires. Investigate
  before classifying a defect, and preserve authorized execution capability.

## Agent workflows and model quality

- [ ] **Run held-out workflows and the independent installation trial.** Existing
  plans distinguish functional regression from controlled agent evaluation.
  Success is repeatable evidence from unfamiliar users and independent clients,
  including failures, recovery, actual provider usage when available, and
  preserved modeling capability. Reuse
  [workflow acceptance #26](https://github.com/chrisbennight/mcp-printable-rs/issues/26),
  [agent evaluation](acceptance/agent-workflows.md), and
  [the installation trial](acceptance/outside-user-trial.md).
- [ ] **Measure reconstruction quality using representative licensed inputs.**
  Random-weight and missing-checkpoint tests establish software behavior rather
  than likeness or manufacturing quality. Success separates geometry, likeness,
  appearance, observation coverage, and printable feature scale, and retains
  model/input identities and limitations. Coordinate with
  [photograph-to-head quality #117](https://github.com/chrisbennight/mcp-printable-rs/issues/117)
  and its existing sub-issues.
- [ ] **Exercise adversarial agent recovery workflows.** Use harmless isolated
  fixtures to check that untrusted imported text and backend results do not
  cause an agent to expand authority or replay an uncertain mutation. Success
  records actual client behavior alongside server enforcement, without treating
  prompt wording as a security boundary.

## References

- [Architecture](docs/architecture.md), [operations](docs/operations.md), and
  [MCP tool contracts](https://modelcontextprotocol.io/specification/2026-07-28/server/tools).
- [GitHub provenance and SBOM attestations](https://docs.github.com/en/actions/security-for-github-actions/using-artifact-attestations/using-artifact-attestations-to-establish-provenance-for-builds).
- [Versioned OpenSSF OSPS Baseline](https://baseline.openssf.org/versions/2026-08-28.html),
  as an assessment reference rather than a claim of complete compliance.
- [CADBench](https://arxiv.org/abs/2605.10873v2), for independent geometric,
  execution, and program-quality measurements across varied inputs.
- [MCPSecBench](https://arxiv.org/abs/2508.13220v3), for testing multiple MCP
  trust boundaries rather than relying only on server unit tests.
