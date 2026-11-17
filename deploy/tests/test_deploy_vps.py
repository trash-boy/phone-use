import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock


spec = importlib.util.spec_from_file_location("deploy_vps", Path(__file__).parents[1] / "deploy-vps.py")
deploy_vps = importlib.util.module_from_spec(spec)
spec.loader.exec_module(deploy_vps)

REVISION = "a" * 40
IMAGE = "ghcr.io/trash-boy/phone-use@sha256:" + "b" * 64
COMPOSE = """services:
  control:
    image: old-image:keep-me
    network_mode: host
    env_file: .env
    environment:
      AI_OBSERVATION: image
    volumes:
      - /srv/containers/iphone-control:/app/.runtime
"""


def request():
    return dict(image=IMAGE, revision=REVISION, branch="release-20261112", runId="123", attempt="1",
                registryUser="trash-boy", registryToken="temporary_token")


class DeployTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.stack = Path(self.directory.name)
        (self.stack / "compose.yaml").write_text(COMPOSE)
        (self.stack / ".env").write_text("CONTROL_ACCESS_TOKEN=private-server-token\n")
        (self.stack / "deployment.json").write_text('{"sourceCommit":"old"}\n')
        self.calls = []

        def run(args, **kwargs):
            self.calls.append((args, kwargs))
            if args[:3] == ["docker", "image", "inspect"]:
                return json.dumps([{"Id": "new-image-id", "Config": {"Labels": {
                    "org.opencontainers.image.revision": REVISION}}}])
            if args[:2] == ["docker", "inspect"]:
                return json.dumps([{"Image": "new-image-id", "State": {"Running": True}}])
            if "ps" in args:
                return "container-id\n"
            return ""

        self.run = Mock(side_effect=run)
        self.health = Mock()

    def deploy(self, payload=None):
        return deploy_vps.deploy(payload or request(), self.stack, self.run, self.health)

    def test_success_preserves_config_and_records_exact_revision(self):
        result = self.deploy()
        self.assertEqual((self.stack / "compose.yaml").read_text(), COMPOSE.replace("old-image:keep-me", IMAGE))
        self.assertEqual((self.stack / ".env").read_text(), "CONTROL_ACCESS_TOKEN=private-server-token\n")
        self.assertEqual(result["sourceCommit"], REVISION)
        self.assertEqual(result["image"], IMAGE)
        self.assertEqual(result["previousImage"], "old-image:keep-me")
        self.assertEqual(json.loads((self.stack / "deployment.json").read_text()), result)
        backup = self.stack / "ci-deployments" / "123-1"
        self.assertEqual((backup / "compose.before.yaml").read_text(), COMPOSE)
        self.assertEqual(json.loads((backup / "deployment.before.json").read_text()), {"sourceCommit": "old"})
        self.assertNotIn("temporary_token", "".join(p.read_text() for p in self.stack.rglob("*") if p.is_file()))
        login = next((args, kwargs) for args, kwargs in self.calls if "login" in args)
        self.assertEqual(login[1]["input"], "temporary_token")
        self.assertNotIn("temporary_token", " ".join(login[0]))
        config_dir = login[0][2]
        self.assertFalse(Path(config_dir).exists())

    def test_opaque_registry_token_reaches_docker_stdin_without_becoming_an_argument(self):
        payload = request()
        payload["registryToken"] = "opaque.token-with+/=~punctuation"
        self.deploy(payload)
        login = next((args, kwargs) for args, kwargs in self.calls if "login" in args)
        self.assertEqual(login[1]["input"], payload["registryToken"])
        self.assertNotIn(payload["registryToken"], " ".join(login[0]))

    def test_failed_health_restores_compose_and_record_and_restarts(self):
        self.health.side_effect = [RuntimeError("new image unhealthy"), None]
        with self.assertRaisesRegex(RuntimeError, "rolled back"):
            self.deploy()
        self.assertEqual((self.stack / "compose.yaml").read_text(), COMPOSE)
        self.assertEqual(json.loads((self.stack / "deployment.json").read_text()), {"sourceCommit": "old"})
        self.assertEqual(sum("up" in args for args, _ in self.calls), 2)
        self.assertEqual(self.health.call_count, 2)

    def test_rollback_failure_is_reported(self):
        self.health.side_effect = [RuntimeError("unhealthy"), RuntimeError("rollback unhealthy")]
        with self.assertRaisesRegex(RuntimeError, "rollback also failed"):
            self.deploy()
        self.assertEqual((self.stack / "compose.yaml").read_text(), COMPOSE)

    def test_invalid_inputs_are_rejected_before_commands_or_writes(self):
        for key, value in [("image", "evil.example/image@sha256:" + "b" * 64),
                           ("image", "ghcr.io/trash-boy/phone-use:latest"),
                           ("revision", "HEAD; rm -rf /"), ("branch", "main"),
                           ("branch", "release-x/subbranch"), ("branch", "release-"),
                           ("runId", "../escape"), ("attempt", "0"),
                           ("registryUser", "--config=evil"), ("registryToken", "a\nb"),
                           ("registryToken", ""), ("registryToken", "a" * 8193),
                           ("registryToken", "a\x00b")]:
            with self.subTest(key=key, value=value):
                payload = request()
                payload[key] = value
                with self.assertRaises(ValueError):
                    self.deploy(payload)
                self.run.assert_not_called()
                self.assertEqual((self.stack / "compose.yaml").read_text(), COMPOSE)

    def test_image_revision_mismatch_does_not_restart_or_change_compose(self):
        self.run.side_effect = lambda args, **kwargs: json.dumps([{"Id": "id", "Config": {"Labels": {
            "org.opencontainers.image.revision": "wrong"}}}]) if args[:3] == ["docker", "image", "inspect"] else ""
        with self.assertRaisesRegex(RuntimeError, "revision"):
            self.deploy()
        self.assertEqual((self.stack / "compose.yaml").read_text(), COMPOSE)
        self.assertFalse(any("up" in args for args, _ in self.calls))

    def test_config_changed_during_pull_is_not_overwritten(self):
        original = self.run.side_effect

        def concurrent_change(args, **kwargs):
            result = original(args, **kwargs)
            if "pull" in args:
                (self.stack / "compose.yaml").write_text(COMPOSE + "# external deployment\n")
            return result

        self.run.side_effect = concurrent_change
        with self.assertRaisesRegex(RuntimeError, "changed"):
            self.deploy()
        self.assertTrue((self.stack / "compose.yaml").read_text().endswith("# external deployment\n"))

    def test_ambiguous_compose_is_rejected(self):
        for source in [COMPOSE + "  other:\n    image: other\n", COMPOSE.replace("  control:", "  other:")]:
            with self.subTest(source=source):
                with self.assertRaises(ValueError):
                    deploy_vps.replace_image(source, IMAGE)

    def missing_previous(self, pull_ok):
        original = self.run.side_effect
        state = {"pulled": False}

        def run(args, **kwargs):
            if args[-1] == "old-image:keep-me":
                if "pull" in args:
                    self.calls.append((args, kwargs))
                    if not pull_ok:
                        raise deploy_vps.subprocess.CalledProcessError(1, args)
                    state["pulled"] = True
                    return ""
                if args[:3] == ["docker", "image", "inspect"] and not state["pulled"]:
                    self.calls.append((args, kwargs))
                    raise deploy_vps.subprocess.CalledProcessError(1, args)
            return original(args, **kwargs)

        self.run.side_effect = run

    def test_missing_rollback_image_that_cannot_be_pulled_aborts_before_changes(self):
        self.missing_previous(pull_ok=False)
        with self.assertRaisesRegex(RuntimeError, "Rollback image old-image:keep-me is not available"):
            self.deploy()
        self.assertEqual((self.stack / "compose.yaml").read_text(), COMPOSE)
        self.assertFalse(any(IMAGE in args for args, _ in self.calls if "pull" in args))
        self.assertFalse(any("up" in args for args, _ in self.calls))
        self.assertFalse((self.stack / "ci-deployments").exists())

    def test_missing_rollback_image_is_pulled_with_the_job_credentials(self):
        self.missing_previous(pull_ok=True)
        self.deploy()
        pull = next(args for args, _ in self.calls if "pull" in args and args[-1] == "old-image:keep-me")
        login = next(args for args, _ in self.calls if "login" in args)
        self.assertEqual(pull[:3], login[:3], "uses the same temporary docker config")
        self.assertEqual((self.stack / "compose.yaml").read_text(), COMPOSE.replace("old-image:keep-me", IMAGE))

    def test_public_url_override_drives_health_checks_and_record(self):
        checked = []
        original = deploy_vps.http_check
        deploy_vps.http_check = lambda url, expected, **kw: checked.append(url)
        self.addCleanup(setattr, deploy_vps, "http_check", original)
        result = deploy_vps.deploy(request(), self.stack, self.run, public_url="https://phone.example.com/x/")
        self.assertEqual(result["publicUrl"], "https://phone.example.com/x/")
        self.assertIn("https://phone.example.com/x/", checked)
        self.assertIn("https://phone.example.com/api/config", checked)
        self.assertNotIn("amazingly.top", " ".join(checked))

    def test_public_url_comes_from_cli_then_environment_then_default(self):
        env = {deploy_vps.PUBLIC_URL_ENV: "https://env.example.com/"}
        self.assertEqual(deploy_vps.parse_args([], {}).public_url, deploy_vps.PUBLIC_URL)
        self.assertEqual(deploy_vps.parse_args([], env).public_url, "https://env.example.com/")
        self.assertEqual(deploy_vps.parse_args(["--public-url", "https://cli.example.com/"], env).public_url,
                         "https://cli.example.com/")
        for bad in ["http://plain.example.com/", "https://user:pw@example.com/", "https://example.com/?q=1"]:
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                deploy_vps.parse_args(["--public-url", bad], {})

    def test_retry_attempt_has_its_own_backup(self):
        self.deploy()
        payload = request()
        payload["attempt"] = "2"
        self.deploy(payload)
        self.assertTrue((self.stack / "ci-deployments" / "123-1" / "compose.before.yaml").is_file())
        self.assertTrue((self.stack / "ci-deployments" / "123-2" / "compose.before.yaml").is_file())


if __name__ == "__main__":
    unittest.main()
