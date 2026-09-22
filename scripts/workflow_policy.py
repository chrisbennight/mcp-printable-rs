"""Validate the trust boundary of GitHub CI and manual release workflows."""

from pathlib import Path
import re

import yaml


def read_workflow(path):
    # BaseLoader preserves GitHub's `on` key and expression strings as written.
    return yaml.load(path.read_text(), Loader=yaml.BaseLoader)


def validate(root):
    failures = []

    def require(condition, message):
        if not condition:
            failures.append(message)

    release = read_workflow(root / ".github/workflows/release.yml")
    publication = read_workflow(root / ".github/workflows/publish-release.yml")
    ci = read_workflow(root / ".github/workflows/ci.yml")
    for workflow in (release, publication):
        require(set(workflow["on"]) == {"workflow_dispatch"}, "release must be manually dispatched")
        require(workflow["concurrency"]["cancel-in-progress"] == "false",
                "an active release must not be cancelled by a later dispatch")
    require(set(release["jobs"]) == {"build"} and set(publication["jobs"]) == {"publish"},
            "public workflows must contain only hosted build and publication jobs")
    build, publish = release["jobs"]["build"], publication["jobs"]["publish"]
    for job in (build, publish):
        require(job["if"] == "github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main' && github.event.repository.private == false",
                "release must require a manual main-branch dispatch in a public repository")
    require(build["runs-on"] == publish["runs-on"] == "ubuntu-latest",
            "building and publishing must use hosted runners")
    require(build["permissions"] == {"contents": "read", "packages": "write"},
            "candidate token permissions changed")
    require(publish["permissions"] == {"contents": "read", "actions": "read", "packages": "write"},
            "publisher token permissions changed")
    require(build["env"]["CRATES_INDEX_URL"] == "${{ vars.CRATES_INDEX_URL }}",
            "release must receive the configured crate proxy")
    require(any(step.get("run") == "./build-docker.sh --push-candidate" for step in build["steps"]),
            "build must run the audited candidate publisher")
    require(publish["steps"][-1].get("run") == 'python3 scripts/release_authorization.py publish "$CANDIDATE_RUN_ID" target/release/candidate.json',
            "record publication must validate trusted candidate-specific GPU approval")
    require(publish["env"].get("QUALIFIER_ID") == "${{ vars.PRINTABLE_QUALIFIER_ID }}",
            "GPU approval must come from the configured trusted account")
    require(any(step.get("id") == "candidate" and step.get("run") ==
                'python3 scripts/release_authorization.py prepare "$CANDIDATE_RUN_ID" >> "$GITHUB_OUTPUT"'
                for step in publish["steps"]), "candidate build identity must be checked")
    require(any(step.get("with", {}).get("ref") == "${{ steps.candidate.outputs.revision }}"
                for step in publish["steps"]), "publication must check out the verified candidate revision")
    require(build["steps"][0]["with"].get("ref") == "${{ github.sha }}",
            "build must check out the dispatched main revision")
    for workflow in (ci, release, publication):
        require(workflow["permissions"] == {"contents": "read"}, "workflow defaults must be read-only")
        for current in workflow["jobs"].values():
            for step in current["steps"]:
                action = step.get("uses")
                if action:
                    require(re.fullmatch(r"[\w./-]+@[a-f0-9]{40}", action) is not None,
                            "actions must use immutable commits")
                if action and action.startswith("actions/checkout@"):
                    require(step.get("with", {}).get("persist-credentials") == "false",
                            "checkout must not persist credentials")
    require(set(ci["on"]) == {"pull_request", "push", "workflow_dispatch"},
            "CI trigger boundary changed")
    for current in ci["jobs"].values():
        require(current["runs-on"] == "ubuntu-latest", "untrusted CI requires hosted runners")
        require("environment" not in current and "permissions" not in current,
                "untrusted CI must not acquire release authority")
        require("secrets." not in str(current), "untrusted CI must not request secrets")
    script = (root / "build-docker.sh").read_text()
    require('CRATES_INDEX_URL is unset; refusing to publish' in script,
            "publishing must refuse an absent crate proxy")
    require('docker buildx imagetools create' not in script,
            "publisher must not change mutable image channels")
    require(not re.search(r"\bdocker\s+(?:tag|push)\b", script),
            "publisher contains an unaudited tag or push")
    require(len(re.findall(r"docker buildx build[^\n]*--push", script)) == 5,
            "publisher must have only the server, Blender, CAD, slicer, and release-record publication paths")
    markers = [
        'cargo "${cargo_index_args[@]}" test',
        'server_commit_tag="${BASE}:sha-${short_sha}"',
        'blender_commit_tag="${BLENDER_BASE}:sha-${short_sha}"',
        'python3 scripts/verify_release_image.py server "$verified_server" "$revision"',
        'python3 scripts/verify_release_image.py blender "$verified_blender" "$revision"',
        'python3 scripts/verify_release_image.py cad "$verified_cad" "$revision"',
        'python3 scripts/verify_release_image.py slicer "$verified_slicer" "$revision"',
        'python3 scripts/release_security.py "$verified_server" "$verified_blender" "$verified_cad" "$verified_slicer"',
        '  /opt/printable/slicer-smoke.py',
        '  /opt/printable/cad/smoke.py --worker',
        'smoke "$verified_server" linux/amd64 "$smoke_port"',
        '  scripts/smoke-release-pair \\' ,
        'python3 scripts/test-image-notices.py "$verified_server" "$verified_blender"',
        'python3 scripts/test-installation.py "$verified_server" "$verified_blender"',
        '  scripts/smoke-blender-gpu "$verified_blender"',
        'pair_commit_tag="${RELEASE_BASE}:sha-${short_sha}"',
        'pair_ref="${pair_commit_tag}@${pair_digest}"',
    ]
    positions = [script.find(marker) for marker in markers]
    require(all(position >= 0 for position in positions) and positions == sorted(positions),
            "tests, immutable image checks, security and GPU qualification must precede pair publication")
    require(".Manifest.Digest" not in script, "registry digests must come from the registry response")
    require("linux/arm64" not in script and "setup-qemu" not in script,
            "release must target Linux/amd64")
    return failures


if __name__ == "__main__":
    root = Path(__file__).resolve().parent.parent
    try:
        failures = validate(root)
    except (KeyError, TypeError, ValueError, yaml.YAMLError) as error:
        raise SystemExit("Invalid workflow structure: " + type(error).__name__) from None
    for failure in failures:
        print("workflow policy: " + failure)
    raise SystemExit(bool(failures))
