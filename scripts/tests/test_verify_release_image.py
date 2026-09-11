from __future__ import annotations

import copy
import unittest

import verify_release_image
from release_identity import ReleaseIdentity


REVISION = "a" * 40
SERVER_IMAGE = (
    "ghcr.io/chrisbennight/mcp-printable-rs:"
    f"sha-{REVISION[:12]}@sha256:{'b' * 64}"
)


def server_document() -> dict:
    contract = verify_release_image.CONTRACTS["server"]
    return {
        "Os": "linux",
        "Architecture": "amd64",
        "Config": {
            "User": contract.user,
            "Healthcheck": {"Test": contract.healthcheck},
            "Entrypoint": contract.entrypoint,
            "Env": list(contract.environment),
            "Labels": {
                "org.opencontainers.image.source": verify_release_image.SOURCE_REPOSITORY,
                "org.opencontainers.image.revision": REVISION,
                "org.printable.role": "server",
            },
        },
    }


class VerifyReleaseImageTests(unittest.TestCase):
    def test_custom_registry_preserves_source_role_and_revision_checks(self):
        identity = ReleaseIdentity("registry.example:5443", "team/tools", "https://example.com/team/tools")
        image = identity.repository("server") + f":sha-{REVISION[:12]}@sha256:{'b' * 64}"
        document = server_document()
        document["Config"]["Labels"]["org.opencontainers.image.source"] = identity.source
        self.assertEqual(verify_release_image.verify("server", image, REVISION, document, [], identity), [])
        for wrong_image in (image.replace("mcp-printable-rs", "mcp-printable-blender"), image.split("@")[0], SERVER_IMAGE):
            self.assertIn("reference is not an immutable server release image",
                          verify_release_image.verify("server", wrong_image, REVISION, document, [], identity))
        document["Config"]["Labels"]["org.opencontainers.image.source"] = ReleaseIdentity.source
        self.assertIn("image label org.opencontainers.image.source does not match the release contract",
                      verify_release_image.verify("server", image, REVISION, document, [], identity))
        self.assertIn("image label org.opencontainers.image.revision does not match the release contract",
                      verify_release_image.verify("server", image, "c" * 40, document, [], identity))

    def test_valid_exact_server_image_passes(self) -> None:
        self.assertEqual(
            verify_release_image.verify(
                "server",
                SERVER_IMAGE,
                REVISION,
                server_document(),
                ["/bin/sh -c build --compat=linux/amd64"],
            ),
            [],
        )

    def test_architecture_contract_and_embedded_credentials_fail(self) -> None:
        document = copy.deepcopy(server_document())
        document["Architecture"] = "arm64"
        document["Config"]["Env"].append("API_TOKEN=embedded-value")

        failures = verify_release_image.verify(
            "server",
            SERVER_IMAGE,
            REVISION,
            document,
            ["RUN registry_password=embedded-value"],
        )

        self.assertIn("image architecture is not amd64", failures)
        self.assertIn(
            "image environment embeds a value for sensitive key API_TOKEN",
            failures,
        )
        self.assertIn(
            "image build history appears to embed credential material",
            failures,
        )

    def test_credential_shaped_values_are_rejected_independent_of_key_name(self) -> None:
        cases = (
            (
                ["DATABASE_URL=postgres://user:password@database.local/app"],
                {},
                [],
                "image environment entry DATABASE_URL contains credential-shaped material",
            ),
            (
                ["REDIS_URL=redis://:password@cache.local/0"],
                {},
                [],
                "image environment entry REDIS_URL contains credential-shaped material",
            ),
            (
                ["PGPASSWORD=embedded-value"],
                {},
                [],
                "image environment embeds a value for sensitive key PGPASSWORD",
            ),
            (
                ["MYSQL_PWD=embedded-value"],
                {},
                [],
                "image environment embeds a value for sensitive key MYSQL_PWD",
            ),
            (
                [],
                {
                    "org.example.connection": (
                        '{"database":"postgres://user:password@database.local/app"}'
                    )
                },
                [],
                "image label org.example.connection contains credential-shaped material",
            ),
            (
                [],
                {"org.example.metadata": '{"password":"embedded-value"}'},
                [],
                "image label org.example.metadata contains credential-shaped material",
            ),
            (
                ["EXTRA_HEADER=Bearer abcdefghijklmnop.qrstuvwxyz"],
                {},
                [],
                "image environment entry EXTRA_HEADER contains credential-shaped material",
            ),
            (
                [],
                {"org.example.metadata": "-----BEGIN PRIVATE KEY-----"},
                [],
                "image label org.example.metadata contains credential-shaped material",
            ),
            (
                [],
                {},
                [
                    "RUN client --password embedded-value",
                    "RUN client --token=embedded-value",
                    "RUN client --password ${DATABASE_PASSWORD:-embedded-value}",
                ],
                "image build history appears to embed credential material",
            ),
        )
        for env, labels, history, expected in cases:
            document = copy.deepcopy(server_document())
            document["Config"]["Env"].extend(env)
            document["Config"]["Labels"].update(labels)

            with self.subTest(expected=expected):
                failures = verify_release_image.verify(
                    "server",
                    SERVER_IMAGE,
                    REVISION,
                    document,
                    history,
                )
                self.assertIn(expected, failures)

        document = copy.deepcopy(server_document())
        self.assertEqual(
            verify_release_image.verify(
                "server",
                SERVER_IMAGE,
                REVISION,
                document,
                ["RUN client --password ${DATABASE_PASSWORD}"],
            ),
            [],
        )

    def test_extra_labels_are_rejected_and_sensitive_names_are_classified(self) -> None:
        for key in (
            "org.example.token",
            "org-example-api-key",
            "org_example.private_key",
        ):
            document = copy.deepcopy(server_document())
            document["Config"]["Labels"][key] = "embedded-value"

            with self.subTest(key=key):
                failures = verify_release_image.verify(
                    "server",
                    SERVER_IMAGE,
                    REVISION,
                    document,
                    [],
                )
                self.assertIn(
                    f"image labels embed a value for sensitive key {key}",
                    failures,
                )

        document = copy.deepcopy(server_document())
        document["Config"]["Labels"]["org.example.compat"] = "linux/amd64"
        self.assertIn(
            "image labels do not match the exact runtime contract",
            verify_release_image.verify(
                "server", SERVER_IMAGE, REVISION, document, []
            ),
        )


if __name__ == "__main__":
    unittest.main()
