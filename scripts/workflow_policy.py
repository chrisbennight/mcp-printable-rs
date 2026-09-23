"""Check CI publication permissions and test dependencies."""
from pathlib import Path
import re
import yaml

def read_workflow(path):
    # Preserve GitHub's on key and expression strings.
    return yaml.load(path.read_text(), Loader=yaml.BaseLoader)

def validate(root):
    failures = []
    def require(condition, message):
        if not condition:
            failures.append(message)
    ci = read_workflow(root / ".github/workflows/ci.yml")
    require(set(ci["on"]) == {"pull_request", "push", "workflow_dispatch"}, "unexpected CI triggers")
    require(ci["on"]["push"]["branches"] == ["main"], "push CI must target main")
    require(ci["permissions"] == {"contents": "read"}, "default permissions must be read-only")
    publish = ci["jobs"]["publish"]
    require(publish["if"] == "github.event_name == 'push' && github.ref == 'refs/heads/main'",
            "only a main push may publish")
    require(set(publish["needs"]) == set(ci["jobs"]) - {"publish"}, "publication must wait for all checks")
    require(publish["permissions"] == {"contents": "read", "packages": "write"}, "unexpected publication permissions")
    require(ci["concurrency"]["cancel-in-progress"] == "${{ github.event_name == 'pull_request' }}",
            "do not cancel main publication for a later push")
    for name, job in ci["jobs"].items():
        require(job["runs-on"] == "ubuntu-latest", "CI must use GitHub-hosted runners")
        require("environment" not in job, "CI must not require environment approval")
        if name != "publish":
            require("permissions" not in job and "secrets." not in str(job),
                    "test jobs must not acquire publishing credentials")
        for step in job["steps"]:
            action = step.get("uses", "")
            if action:
                require(re.fullmatch(r"[\w./-]+@[a-f0-9]{40}", action) is not None, "pin actions to commits")
            if action.startswith("actions/checkout@"):
                require(step.get("with", {}).get("persist-credentials") == "false", "do not persist Git credentials")
    container_steps = str(ci["jobs"]["containers"]["steps"])
    for marker in ("cad/smoke.py --worker", "slicer-smoke.py", "scripts/test-installation.py",
                   "scripts/test-image-notices.py", "scripts/smoke-blender-cpu", "scripts/smoke-release-pair",
                   "scripts/publish_images.py record", "docker save", "image-ids.json"):
        require(marker in container_steps, "missing container check or tested image transfer: " + marker)
    publication_steps = str(publish["steps"])
    for marker in ("actions/download-artifact@", "tested-images", "docker load",
                   "scripts/publish_images.py publish"):
        require(marker in publication_steps, "missing tested-image publication step: " + marker)
    require("docker build" not in publication_steps, "publication must not rebuild runtime images")
    require(not (root / ".github/workflows/release.yml").exists()
            and not (root / ".github/workflows/publish-release.yml").exists(),
            "obsolete separate release workflows must be removed")
    return failures

if __name__ == "__main__":
    try:
        failures = validate(Path(__file__).resolve().parent.parent)
    except (KeyError, TypeError, ValueError, yaml.YAMLError) as error:
        raise SystemExit("Invalid workflow structure: " + type(error).__name__) from None
    for failure in failures:
        print("workflow policy: " + failure)
    raise SystemExit(bool(failures))
