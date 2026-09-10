from __future__ import annotations

import copy
import io
import subprocess
import unittest
from contextlib import redirect_stderr, redirect_stdout
from unittest import mock

import release_security


class ReleaseSecurityTests(unittest.TestCase):
    def setUp(self) -> None:
        self.policy = {
            "scanner_version": "0.110.0",
            "fail_on_severities": ["critical", "high"],
            "severity_requires_fix": True,
            "fail_on_kev": True,
            "ignored_vulnerabilities": [],
        }

    def test_evaluate_blocks_high_severity_and_related_kev(self) -> None:
        report = {
            "matches": [
                {
                    "vulnerability": {
                        "id": "GHSA-high",
                        "severity": "High",
                        "fix": {"state": "fixed", "versions": ["2.0.0"]},
                    },
                    "relatedVulnerabilities": [{"id": "CVE-2026-1000"}],
                },
                {
                    "vulnerability": {
                        "id": "CVE-2026-2000",
                        "severity": "Low",
                        "fix": {"state": "not-fixed", "versions": []},
                    }
                },
            ]
        }

        findings = release_security.evaluate(
            report,
            self.policy,
            {"CVE-2026-2000"},
        )

        self.assertEqual(
            findings,
            [
                release_security.ActionableFinding(
                    "CVE-2026-1000", "high", "severity"
                ),
                release_security.ActionableFinding(
                    "CVE-2026-2000", "low", "kev"
                ),
                release_security.ActionableFinding(
                    "GHSA-high", "high", "severity"
                ),
            ],
        )

    def test_ignore_list_cannot_suppress_a_known_exploited_vulnerability(self) -> None:
        policy = copy.deepcopy(self.policy)
        policy["ignored_vulnerabilities"] = ["CVE-2026-3000"]
        report = {
            "matches": [
                {
                    "vulnerability": {
                        "id": "CVE-2026-3000",
                        "severity": "Critical",
                        "fix": {"state": "fixed", "versions": ["2.0.0"]},
                    }
                }
            ]
        }

        self.assertEqual(
            release_security.evaluate(report, policy, {"CVE-2026-3000"}),
            [
                release_security.ActionableFinding(
                    "CVE-2026-3000", "critical", "kev"
                )
            ],
        )

    def test_evaluate_rejects_matches_without_a_vulnerability_identifier(self) -> None:
        for match in (
            {},
            {
                "vulnerability": {
                    "severity": "Low",
                    "fix": {"state": "unknown", "versions": []},
                }
            },
            {
                "vulnerability": {
                    "id": "",
                    "severity": "High",
                    "fix": {"state": "not-fixed", "versions": []},
                },
                "relatedVulnerabilities": [{"id": ""}],
            },
        ):
            with self.subTest(match=match), self.assertRaises(ValueError):
                release_security.evaluate(
                    {"matches": [match]},
                    self.policy,
                    set(),
                )

    def test_evaluate_rejects_invalid_vulnerability_severity(self) -> None:
        for severity in (None, 7, "", "urgent"):
            with self.subTest(severity=severity), self.assertRaises(ValueError):
                release_security.evaluate(
                    {
                        "matches": [
                            {
                                "vulnerability": {
                                    "id": "CVE-2026-5000",
                                    "severity": severity,
                                    "fix": {"state": "fixed", "versions": ["2.0.0"]},
                                }
                            }
                        ]
                    },
                    self.policy,
                    set(),
                )

    def test_unremediated_severity_is_visible_and_only_kev_blocks(self) -> None:
        report = {
            "matches": [
                {
                    "vulnerability": {
                        "id": "CVE-2026-6000",
                        "severity": "Critical",
                        "fix": {"state": "not-fixed", "versions": []},
                    }
                }
            ]
        }

        evaluation = release_security.evaluate_report(report, self.policy, set())
        self.assertEqual(evaluation.blocking, ())
        self.assertEqual(
            evaluation.unremediated,
            (
                release_security.UnremediatedFinding(
                    "CVE-2026-6000", "critical", "not-fixed"
                ),
            ),
        )
        stdout = io.StringIO()
        stderr = io.StringIO()
        with redirect_stdout(stdout), redirect_stderr(stderr):
            passed = release_security.emit_evaluation(
                "printable@example",
                evaluation,
            )
        self.assertTrue(passed)
        self.assertEqual(stderr.getvalue(), "")
        self.assertIn(
            "RELEASE_SCAN_UNREMEDIATED image=printable@example "
            "vulnerability=CVE-2026-6000 severity=critical fix_state=not-fixed",
            stdout.getvalue(),
        )
        self.assertIn("RELEASE_SCAN_OK image=printable@example", stdout.getvalue())
        self.assertEqual(
            release_security.evaluate(report, self.policy, {"CVE-2026-6000"}),
            [
                release_security.ActionableFinding(
                    "CVE-2026-6000", "critical", "kev"
                )
            ],
        )

    def test_kev_feed_fails_closed_when_empty_or_malformed(self) -> None:
        for payload in (
            {},
            {"vulnerabilities": []},
            {"vulnerabilities": [{}]},
            {"vulnerabilities": [{"cveID": "GHSA-not-a-cve"}]},
            {"vulnerabilities": [{"cveID": "cve-2026-4000"}]},
            {"vulnerabilities": [{"cveID": "CVE-26-1"}]},
        ):
            with self.subTest(payload=payload), self.assertRaises(RuntimeError):
                release_security.parse_kev_ids(payload)

        self.assertEqual(
            release_security.parse_kev_ids(
                {"vulnerabilities": [{"cveID": "CVE-2026-4000"}]}
            ),
            {"CVE-2026-4000"},
        )

    def test_scanner_version_must_match_the_release_policy(self) -> None:
        with mock.patch.object(
            release_security.subprocess,
            "run",
            return_value=subprocess.CompletedProcess(
                ["grype", "version"],
                0,
                stdout="Application: grype\nVersion: 0.110.0\n",
                stderr="",
            ),
        ) as run:
            release_security.verify_grype_version("0.110.0")
        run.assert_called_once_with(
            ["grype", "version"],
            check=True,
            capture_output=True,
            text=True,
            timeout=30,
        )

        for output in (
            "Application: grype\nVersion: 0.109.0\n",
            "grype 0.110.0\n",
        ):
            with (
                self.subTest(output=output),
                mock.patch.object(
                    release_security.subprocess,
                    "run",
                    return_value=subprocess.CompletedProcess(
                        ["grype", "version"],
                        0,
                        stdout=output,
                        stderr="",
                    ),
                ),
                self.assertRaises(RuntimeError),
            ):
                release_security.verify_grype_version("0.110.0")


if __name__ == "__main__":
    unittest.main()
