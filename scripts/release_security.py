#!/usr/bin/env python3
"""Scan exact Printable release images with the fleet's Grype policy."""

from __future__ import annotations

import json
import re
import subprocess
import sys
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from release_identity import ReleaseIdentity

ROOT = Path(__file__).resolve().parent.parent
POLICY_PATH = ROOT / "release" / "security-policy.json"
KEV_URL = (
    "https://www.cisa.gov/sites/default/files/feeds/"
    "known_exploited_vulnerabilities.json"
)
GRYPE_TIMEOUT_SECONDS = 900
GRYPE_VERSION_RE = re.compile(r"(?m)^Version:\s*(\S+)\s*$")
CVE_ID_RE = re.compile(r"^CVE-\d{4}-\d{4,}$")
GRYPE_SEVERITIES = {
    "unknown",
    "negligible",
    "low",
    "medium",
    "high",
    "critical",
}
GRYPE_FIX_STATES = {"fixed", "not-fixed", "wont-fix", "unknown"}


@dataclass(frozen=True, order=True)
class ActionableFinding:
    vulnerability_id: str
    severity: str
    reason: str


@dataclass(frozen=True, order=True)
class UnremediatedFinding:
    vulnerability_id: str
    severity: str
    fix_state: str


@dataclass(frozen=True)
class ScanEvaluation:
    blocking: tuple[ActionableFinding, ...]
    unremediated: tuple[UnremediatedFinding, ...]


def load_policy() -> dict[str, Any]:
    payload = json.loads(POLICY_PATH.read_text(encoding="utf-8"))
    severities = payload.get("fail_on_severities")
    ignored = payload.get("ignored_vulnerabilities")
    if (
        not isinstance(payload.get("scanner_version"), str)
        or not re.fullmatch(r"\d+\.\d+\.\d+", payload["scanner_version"])
        or not isinstance(severities, list)
        or not severities
        or any(not isinstance(value, str) for value in severities)
        or any(value.lower() not in GRYPE_SEVERITIES for value in severities)
        or not isinstance(payload.get("severity_requires_fix"), bool)
        or not isinstance(payload.get("fail_on_kev"), bool)
        or not isinstance(ignored, list)
        or any(not isinstance(value, str) or not value for value in ignored)
    ):
        raise ValueError("release security policy has an invalid shape")
    return payload


def verify_grype_version(expected: str) -> None:
    result = subprocess.run(
        ["grype", "version"],
        check=True,
        capture_output=True,
        text=True,
        timeout=30,
    )
    match = GRYPE_VERSION_RE.search(result.stdout)
    if match is None:
        raise RuntimeError("Grype version output is malformed")
    actual = match.group(1)
    if actual != expected:
        raise RuntimeError(f"Grype {expected} is required, found {actual}")


def load_kev_ids() -> set[str]:
    try:
        with urllib.request.urlopen(KEV_URL, timeout=30) as response:
            payload = json.load(response)
    except (OSError, TimeoutError, urllib.error.URLError, json.JSONDecodeError) as error:
        raise RuntimeError("CISA KEV feed is unavailable; release scan fails closed") from error
    return parse_kev_ids(payload)


def parse_kev_ids(payload: Any) -> set[str]:
    vulnerabilities = payload.get("vulnerabilities") if isinstance(payload, dict) else None
    if not isinstance(vulnerabilities, list) or not vulnerabilities:
        raise RuntimeError("CISA KEV feed does not contain a vulnerabilities list")
    identifiers: set[str] = set()
    for item in vulnerabilities:
        if (
            not isinstance(item, dict)
            or not isinstance(item.get("cveID"), str)
            or CVE_ID_RE.fullmatch(item["cveID"]) is None
        ):
            raise RuntimeError("CISA KEV feed contains an invalid vulnerability")
        identifiers.add(item["cveID"])
    return identifiers


def vulnerability_ids(match: dict[str, Any]) -> set[str]:
    vulnerability = match.get("vulnerability")
    vulnerability = vulnerability if isinstance(vulnerability, dict) else {}
    ids = {
        value
        for value in [vulnerability.get("id")]
        if isinstance(value, str) and value
    }
    related = match.get("relatedVulnerabilities")
    for item in related if isinstance(related, list) else []:
        if (
            isinstance(item, dict)
            and isinstance(item.get("id"), str)
            and item["id"]
        ):
            ids.add(item["id"])
    return ids


def vulnerability_fix_state(vulnerability: dict[str, Any]) -> str:
    fix = vulnerability.get("fix")
    if not isinstance(fix, dict):
        raise ValueError("Grype match has no vulnerability fix state")
    state = fix.get("state")
    if not isinstance(state, str) or state not in GRYPE_FIX_STATES:
        raise ValueError("Grype match has an invalid vulnerability fix state")
    return state


def evaluate(
    report: dict[str, Any],
    policy: dict[str, Any],
    kev_ids: set[str],
) -> list[ActionableFinding]:
    return list(evaluate_report(report, policy, kev_ids).blocking)


def evaluate_report(
    report: dict[str, Any],
    policy: dict[str, Any],
    kev_ids: set[str],
) -> ScanEvaluation:
    fail_severities = {
        str(value).lower() for value in policy["fail_on_severities"]
    }
    ignored = set(policy["ignored_vulnerabilities"])
    severity_requires_fix = policy["severity_requires_fix"]
    fail_on_kev = policy["fail_on_kev"]
    actionable: set[ActionableFinding] = set()
    unremediated: set[UnremediatedFinding] = set()
    matches = report.get("matches")
    if not isinstance(matches, list):
        raise ValueError("Grype report has no matches list")

    for match in matches:
        if not isinstance(match, dict):
            raise ValueError("Grype report contains a non-object match")
        vulnerability = match.get("vulnerability")
        vulnerability = vulnerability if isinstance(vulnerability, dict) else {}
        raw_severity = vulnerability.get("severity")
        if (
            not isinstance(raw_severity, str)
            or raw_severity.lower() not in GRYPE_SEVERITIES
        ):
            raise ValueError("Grype match has an invalid vulnerability severity")
        severity = raw_severity.lower()
        fix_state = vulnerability_fix_state(vulnerability)
        identifiers = vulnerability_ids(match)
        if not identifiers:
            raise ValueError("Grype match has no vulnerability identifier")
        for vulnerability_id in identifiers:
            severity_blocked = (
                vulnerability_id not in ignored
                and severity in fail_severities
                and (not severity_requires_fix or fix_state == "fixed")
            )
            if severity_blocked:
                actionable.add(
                    ActionableFinding(vulnerability_id, severity, "severity")
                )
            if severity in fail_severities and fix_state != "fixed":
                unremediated.add(
                    UnremediatedFinding(vulnerability_id, severity, fix_state)
                )
            if fail_on_kev and vulnerability_id in kev_ids:
                actionable.add(ActionableFinding(vulnerability_id, severity, "kev"))
    return ScanEvaluation(
        blocking=tuple(sorted(actionable)),
        unremediated=tuple(sorted(unremediated)),
    )


def emit_evaluation(image: str, evaluation: ScanEvaluation) -> bool:
    for finding in evaluation.unremediated:
        print(
            "RELEASE_SCAN_UNREMEDIATED "
            f"image={image} vulnerability={finding.vulnerability_id} "
            f"severity={finding.severity} fix_state={finding.fix_state}"
        )
    if evaluation.blocking:
        summary = ", ".join(
            f"{finding.vulnerability_id}({finding.severity},{finding.reason})"
            for finding in evaluation.blocking[:50]
        )
        if len(evaluation.blocking) > 50:
            summary += f", and {len(evaluation.blocking) - 50} more"
        print(f"RELEASE_SCAN_BLOCKED image={image} findings={summary}", file=sys.stderr)
        return False
    print(f"RELEASE_SCAN_OK image={image}")
    return True


def scan(image: str) -> dict[str, Any]:
    result = subprocess.run(
        ["grype", image, "--output", "json"],
        check=True,
        capture_output=True,
        text=True,
        timeout=GRYPE_TIMEOUT_SECONDS,
    )
    payload = json.loads(result.stdout)
    if not isinstance(payload, dict):
        raise ValueError("Grype report is not an object")
    return payload


def main() -> int:
    images = sys.argv[1:]
    if not images:
        print(
            "usage: release_security.py <tag@sha256:digest> "
            "[<tag@sha256:digest> ...]",
            file=sys.stderr,
        )
        return 2
    try:
        identity = ReleaseIdentity.from_environment()
    except ValueError:
        print("release registry or source identity is invalid", file=sys.stderr)
        return 2
    patterns = [identity.reference_pattern(role) for role in ("server", "blender")]
    if any(not any(pattern.fullmatch(image) for pattern in patterns) for image in images):
        print("every release scan target must be an exact Printable digest", file=sys.stderr)
        return 2

    try:
        policy = load_policy()
        verify_grype_version(policy["scanner_version"])
        kev_ids = load_kev_ids() if policy["fail_on_kev"] else set()
        blocked = False
        for image in images:
            try:
                evaluation = evaluate_report(scan(image), policy, kev_ids)
            except (
                json.JSONDecodeError,
                OSError,
                subprocess.CalledProcessError,
                subprocess.TimeoutExpired,
                ValueError,
            ) as error:
                blocked = True
                print(
                    f"RELEASE_SCAN_FAILED image={image} error={error}",
                    file=sys.stderr,
                )
                continue
            if not emit_evaluation(image, evaluation):
                blocked = True
        return 1 if blocked else 0
    except (
        json.JSONDecodeError,
        OSError,
        subprocess.CalledProcessError,
        subprocess.TimeoutExpired,
        RuntimeError,
        ValueError,
    ) as error:
        print(f"release security scan failed closed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
