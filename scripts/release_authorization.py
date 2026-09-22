#!/usr/bin/env python3
"""Publish only a successful public candidate with trusted GPU approval."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import urllib.request

from release_candidate import digest, publish, validate
from release_identity import ReleaseIdentity


REPOSITORY = "chrisbennight/mcp-printable-rs"


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise ValueError("GitHub API redirects are not accepted")


def api(path):
    request = urllib.request.Request(
        "https://api.github.com/repos/" + REPOSITORY + path,
        headers={"Authorization": "Bearer " + os.environ["GH_TOKEN"],
                 "Accept": "application/vnd.github+json",
                 "X-GitHub-Api-Version": "2022-11-28"})
    with urllib.request.build_opener(NoRedirect).open(request, timeout=30) as response:
        raw = response.read(2 * 1024 * 1024 + 1)
    if len(raw) > 2 * 1024 * 1024:
        raise ValueError("GitHub API response exceeds the evidence limit")
    return json.loads(raw)


def candidate_run(run_id):
    if not re.fullmatch(r"[1-9][0-9]{0,19}", run_id):
        raise ValueError("candidate run ID must be a positive integer")
    run = api("/actions/runs/" + run_id)
    if (run.get("id") != int(run_id)
            or run.get("repository", {}).get("full_name") != REPOSITORY
            or run.get("repository", {}).get("private") is not False
            or run.get("head_repository", {}).get("full_name") != REPOSITORY
            or run.get("path") != ".github/workflows/release.yml"
            or run.get("event") != "workflow_dispatch"
            or run.get("head_branch") != "main"
            or run.get("status") != "completed"
            or run.get("conclusion") != "success"
            or not re.fullmatch(r"[0-9a-f]{40}", run.get("head_sha", ""))):
        raise ValueError("candidate requires a successful public main-branch release build")
    return run


def require_approval(run_id, candidate, qualifier_id):
    if not re.fullmatch(r"[1-9][0-9]{0,19}", qualifier_id):
        raise ValueError("a trusted qualifier account ID must be configured")
    context = "printable/gpu/" + run_id
    # GitHub returns statuses newest first. A later failure revokes an approval.
    for page in range(1, 21):
        statuses = api("/commits/" + candidate["revision"]
                       + "/statuses?per_page=100&page=" + str(page))
        for status in statuses:
            if status.get("context") != context:
                continue
            if (status.get("creator", {}).get("id") != int(qualifier_id)
                    or status.get("state") != "success"
                    or status.get("description") != "sha256:" + digest(candidate)):
                raise ValueError("latest GPU approval is untrusted, unsuccessful, or for another candidate")
            return
        if len(statuses) < 100:
            break
    raise ValueError("no matching trusted GPU approval")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("prepare", "publish"))
    parser.add_argument("run_id")
    parser.add_argument("candidate", nargs="?")
    args = parser.parse_args()
    run = candidate_run(args.run_id)
    if args.operation == "prepare":
        print("revision=" + run["head_sha"])
        return
    if args.candidate is None:
        parser.error("publish requires a candidate file")
    identity = ReleaseIdentity.from_environment()
    candidate = validate(json.loads(Path(args.candidate).read_text()), identity)
    checkout = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    if candidate["revision"] != run["head_sha"] or checkout != run["head_sha"]:
        raise ValueError("candidate, build, and checkout revisions must match")
    require_approval(args.run_id, candidate, os.environ.get("QUALIFIER_ID", ""))
    publish(candidate, {"candidate_sha256": digest(candidate), "gpu": "passed"}, identity)


if __name__ == "__main__":
    main()
