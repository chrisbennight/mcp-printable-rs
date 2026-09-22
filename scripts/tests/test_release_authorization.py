import copy
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from release_authorization import REPOSITORY, candidate_run, main, require_approval
from release_candidate import ROLES, digest
from release_identity import ReleaseIdentity


class AuthorizationTests(unittest.TestCase):
    def setUp(self):
        identity = ReleaseIdentity()
        self.candidate = {
            "revision": "a" * 40,
            "source": identity.source,
            "images": {role: identity.repository(role) + ":sha-" + "a" * 12
                       + "@sha256:" + str(i) * 64 for i, role in enumerate(ROLES)},
        }
        self.run = {"id": 123, "repository": {"full_name": REPOSITORY, "private": False},
                    "head_repository": {"full_name": REPOSITORY},
                    "path": ".github/workflows/release.yml", "event": "workflow_dispatch",
                    "head_branch": "main", "status": "completed", "conclusion": "success",
                    "head_sha": self.candidate["revision"]}
        self.approval = {"context": "printable/gpu/123", "creator": {"id": 456},
                         "state": "success", "description": "sha256:" + digest(self.candidate)}

    def test_invalid_run_id_never_reaches_api(self):
        with patch("release_authorization.api") as api:
            for run_id in ("0", "../other", "123?x=1", "$(id)"):
                with self.assertRaises(ValueError):
                    candidate_run(run_id)
            api.assert_not_called()

    def test_only_successful_public_main_candidate_builds_are_eligible(self):
        for changes in ({"conclusion": "failure"}, {"status": "in_progress"},
                        {"head_branch": "feature"}, {"event": "pull_request"},
                        {"path": ".github/workflows/ci.yml"}, {"id": 124},
                        {"repository": {"full_name": REPOSITORY, "private": True}},
                        {"head_repository": {"full_name": "other/repository"}}):
            with self.subTest(changes=changes), patch("release_authorization.api", return_value=self.run | changes):
                with self.assertRaises(ValueError):
                    candidate_run("123")
        with patch("release_authorization.api", return_value=self.run):
            self.assertEqual(candidate_run("123")["head_sha"], "a" * 40)

    def test_approval_requires_trusted_creator_exact_candidate_and_run(self):
        for changes in ({"creator": {"id": 789}}, {"state": "failure"},
                        {"description": "sha256:" + "f" * 64},
                        {"context": "printable/gpu/124"}):
            with self.subTest(changes=changes), patch("release_authorization.api", return_value=[self.approval | changes]):
                with self.assertRaises(ValueError):
                    require_approval("123", self.candidate, "456")

    def test_later_failure_revokes_earlier_success(self):
        with patch("release_authorization.api", return_value=[self.approval | {"state": "failure"}, self.approval]):
            with self.assertRaises(ValueError):
                require_approval("123", self.candidate, "456")

    def test_status_pagination_finds_matching_approval(self):
        with patch("release_authorization.api", side_effect=[[{"context": "other"}] * 100, [self.approval]]) as api:
            require_approval("123", self.candidate, "456")
            self.assertTrue(api.call_args.args[0].endswith("page=2"))

    def test_missing_qualifier_fails_closed(self):
        with patch("release_authorization.api") as api:
            with self.assertRaises(ValueError):
                require_approval("123", self.candidate, "")
            api.assert_not_called()

    def test_publication_rejects_changed_images_and_wrong_build_revision(self):
        for change in ("images", "revision", "approval"):
            candidate = copy.deepcopy(self.candidate)
            run = copy.deepcopy(self.run)
            statuses = [self.approval]
            if change == "images":
                candidate["images"]["server"] = candidate["images"]["server"].replace("0" * 64, "f" * 64)
            elif change == "revision":
                run["head_sha"] = "b" * 40
            else:
                statuses = []
            with self.subTest(change=change), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "candidate.json"
                path.write_text(json.dumps(candidate))
                with (patch("sys.argv", ["release_authorization.py", "publish", "123", str(path)]),
                      patch.dict("os.environ", {"QUALIFIER_ID": "456"}),
                      patch("release_authorization.api", side_effect=[run, statuses]),
                      patch("release_authorization.subprocess.check_output", return_value="a" * 40),
                      patch("release_authorization.publish") as publish):
                    with self.assertRaises(ValueError):
                        main()
                    publish.assert_not_called()

    def test_matching_build_and_approval_allow_publication(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "candidate.json"
            path.write_text(json.dumps(self.candidate))
            with (patch("sys.argv", ["release_authorization.py", "publish", "123", str(path)]),
                  patch.dict("os.environ", {"QUALIFIER_ID": "456"}),
                  patch("release_authorization.api", side_effect=[self.run, [self.approval]]),
                  patch("release_authorization.subprocess.check_output", return_value="a" * 40),
                  patch("release_authorization.publish") as publish):
                main()
                self.assertEqual(publish.call_args.args[0], self.candidate)
