# Publication scope and retained history

The GitHub repository is public. Its first commit is a snapshot of the
selected Gitea source tree; earlier Gitea commits and tags were not imported.
Later GitHub migration commits have their own history. This is not a claim that
all historical credentials or metadata have been erased from the original forge.

## Content selected for this repository

The maintained distribution includes the Cargo workspace and lockfile, Blender
add-on, checksum-pinned image recipes, workflow/tool contracts, tests, examples,
standalone installation, and contributor and operator documentation. The
first-party license and upstream notices remain part of that content.

Historical design context is retained without site-specific operating details:

| Material | Disposition |
| --- | --- |
| `PLAN.md`, `DECISIONS.md`, `docs/product-refactor.md` | Historical design decisions with current GitHub tracking; not evidence of current deployment |
| `DEPLOYMENT.md` | Links to maintained installation and operation guides |
| `docs/history/imported-readme.md` | Preserved detailed earlier README, clearly marked as historical |
| `.gitea/` | Historical template and retirement note; obsolete workflows removed |
| `evidence/blender-gpu-smoke-2026-07-18.md`, `acceptance/agent-workflows.md` | Qualification guidance and evaluation contracts without site-specific observations |

Earlier commits may contain deployment metadata that does not belong in a
public example. Review them separately from credential scanning. New changes
must not introduce private hostnames, addresses, registry locations, deployment
identifiers, or site configuration. Preserve detailed operational evidence in
private records rather than reproducing it in public issues or documentation.
Removing information from the current tree does not erase earlier Git commits,
issue edits, or downloaded copies. Replacing a historical hostname with an
example also does not qualify another deployment.

No production Compose stack, bearer file, local environment file, or private
Cargo configuration belongs in the published source set. The standalone Compose
file creates its own dedicated workspace and uses a locally generated bearer.
Build-context checks verify that local configuration stays outside image builds.
Keep author and upstream attribution; assess any personal commit metadata before
changing visibility rather than silently rewriting someone else's authorship.

## Recorded checks and publication requirements

On 11 September 2026, Gitleaks 8.30.1 scanned candidate commit
`14240e74b8bdb18528235f5e205eb1cfbe95b42a` and its reachable GitHub history,
then separately scanned every tracked file exported from its unchanged index.
Both scans exited successfully with no findings. The
[redacted scan record](../evidence/publication-secret-scan.json) identifies the
revision, tool version, scope, and outcomes. No custom exclusions were added;
the scanner's built-in rules and allowlists still apply. Other forge references,
ignored files, and untracked local files were outside that selected publication
set. The original Gitea history was not scanned by this check.

The executed scanner commands were:

```sh
gitleaks git . --log-opts=HEAD --redact=100 --report-format=json --report-path=- --no-banner --no-color --log-level=error
gitleaks dir TRACKED_SOURCE_EXPORT --redact=100 --report-format=json --report-path=- --no-banner --no-color --log-level=error
```

The source export used `git checkout-index --all --prefix=TEMPORARY_DIRECTORY/`
after verifying that both the worktree and index matched the recorded commit.
Scanner output was captured and projected to redacted finding metadata; no
credential values were included in the record. Repeat the checks for later
changes before publication.

Scan the exact selected Git history and tracked source, recording the scanner
version, revision, exclusions, and redacted findings. A secret scanner is a
heuristic check; no findings does not prove that every sensitive value is absent.
Revoke any real exposed credential through its provider before discussing
history cleanup. Never place a discovered value in an issue or scanner summary.

Verify the private vulnerability-reporting path with an external account, as
described in [SECURITY.md](../SECURITY.md). Confirm that package visibility is a
separate decision from repository visibility. Complete the corresponding-source
and notice material in [the license guidance](../THIRD_PARTY_NOTICES.md) before
binary distribution, and retain qualification evidence for the exact image pair.

The automated source checks, tutorial, and software-container exercises are
useful evidence. The [outside-user trial](../acceptance/outside-user-trial.md)
and NVIDIA qualification require their own actual results. Do not describe a
pending trial, an unmeasured latency target, or a planned release as completed.
