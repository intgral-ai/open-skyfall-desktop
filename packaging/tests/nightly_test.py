"""Exercise the real nightly decision script without GitHub or repository mutations."""

import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "nightly.sh"
ROOT = SCRIPT.parent.parent


def release(tag, published, *, draft=False, prerelease=False):
    return {"tag_name": tag, "published_at": published, "draft": draft,
            "prerelease": prerelease, "target_commitish": "main"}


class NightlyDecision(unittest.TestCase):
    def run_decision(self, pages, refs=None, *, event="schedule", sha="current", api_fail=False):
        self.assertIsNotNone(shutil.which("jq"), "install jq to run packaging checks")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            output = root / "output"
            summary = root / "summary"
            calls = root / "calls"
            output.touch()
            summary.touch()
            for command, body in {
                "gh": '''import json, os, sys
args = sys.argv[1:]
with open(os.environ["CALLS"], "a") as log: log.write(json.dumps(args) + "\\n")
if args != ["api", "--paginate", "--slurp", "repos/test/project/releases?per_page=100"]:
    sys.exit("unexpected gh invocation (slurp and jq must not be combined)")
if os.environ["API_FAIL"] == "1": sys.exit("mock API failure")
print(os.environ["PAGES"])
''',
                "git": '''import json, os, sys
args = sys.argv[1:]
with open(os.environ["CALLS"], "a") as log: log.write(json.dumps(args) + "\\n")
refs = json.loads(os.environ["REFS"])
if len(args) != 3 or args[:2] != ["rev-parse", "--verify"] or args[2] not in refs:
    sys.exit("missing tag")
print(refs[args[2]])
''',
            }.items():
                path = root / command
                path.write_text("#!/usr/bin/env python3\n" + body, encoding="utf-8")
                path.chmod(0o755)
            env = {"PATH": str(root) + os.pathsep + os.environ["PATH"],
                   "GITHUB_EVENT_NAME": event, "GITHUB_REPOSITORY": "test/project",
                   "GITHUB_SHA": sha, "GITHUB_OUTPUT": str(output),
                   "GITHUB_STEP_SUMMARY": str(summary), "CALLS": str(calls),
                   "PAGES": json.dumps(pages), "REFS": json.dumps(refs or {}),
                   "API_FAIL": "1" if api_fail else "0"}
            result = subprocess.run(["bash", str(SCRIPT)], env=env, capture_output=True, text=True, check=False)
            return result, output.read_text(), summary.read_text(), calls.read_text() if calls.exists() else ""

    def test_newest_published_prerelease_across_pages_skips_exact_tag_commit(self):
        pages = [[release("v1", "2026-01-01T00:00:00Z")],
                 [release("v2-nightly", "2026-01-03T00:00:00Z", prerelease=True),
                  release("v3-draft", "2026-01-04T00:00:00Z", draft=True)]]
        result, output, summary, calls = self.run_decision(pages, {"refs/tags/v2-nightly^{commit}": "current"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(output, "build=false\n")
        self.assertIn("v2-nightly", summary)
        self.assertIn("refs/tags/v2-nightly^{commit}", calls)
        self.assertNotIn("v3-draft^{commit}", calls)

    def test_changed_commit_builds(self):
        result, output, _, _ = self.run_decision([[release("v1", "2026-01-01T00:00:00Z")]],
                                                {"refs/tags/v1^{commit}": "previous"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(output, "build=true\n")

    def test_no_published_release_builds(self):
        for pages in [[], [[]], [[release("draft", None, draft=True)]]]:
            with self.subTest(pages=pages):
                result, output, _, calls = self.run_decision(pages)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(output, "build=true\n")
                self.assertNotIn("rev-parse", calls)

    def test_non_schedule_does_not_query_github(self):
        for event in ["push", "workflow_dispatch"]:
            with self.subTest(event=event):
                result, output, _, calls = self.run_decision([], event=event, api_fail=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(output, "build=true\n")
                self.assertEqual(calls, "")

    def test_api_failure_does_not_report_build_or_skip(self):
        result, output, _, _ = self.run_decision([], api_fail=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(output, "")

    def test_missing_tag_fails_instead_of_using_moving_target_commitish(self):
        result, output, _, _ = self.run_decision([[release("missing", "2026-01-01T00:00:00Z")]])
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(output, "")


class ForkWorkflowSafety(unittest.TestCase):
    """Catch executable publication paths, not the presence of old helper tests."""

    WORKFLOWS = ("release", "release-gate", "docker", "pages", "flatpak-pages", "snap-store", "winget")

    def workflow(self, name):
        return (ROOT / ".github/workflows" / (name + ".yml")).read_text()

    def test_no_event_or_manual_input_can_reactivate_a_publisher(self):
        for name in self.WORKFLOWS:
            with self.subTest(workflow=name):
                text = self.workflow(name)
                events = text.split("\non:\n", 1)[1].split("\npermissions:", 1)[0]
                self.assertNotRegex(events, r"(?m)^  (release|schedule|push):")
                self.assertNotRegex(events, r"(?m)^      (draft|signpath_signing_policy|packages_only):")
                self.assertNotRegex(text, r"gh release (create|edit|upload|delete)|snapcraft (upload|promote)|--submit\b|gh workflow run")

    def test_builds_never_receive_signing_or_publication_credentials(self):
        for name in self.WORKFLOWS:
            with self.subTest(workflow=name):
                text = self.workflow(name)
                self.assertNotIn("secrets.", text)
                self.assertNotRegex(text, r"(?m)^\s+[\w-]+: write\s*$")
                self.assertNotRegex(text, r"signpath/github-action|docker/login-action|actions/deploy-pages|signing.sh|--sign\b")
                self.assertNotRegex(text, r"(?m)^\s+push: true\s*$")

    def test_disabled_publishers_are_separate_from_build_jobs(self):
        for name, job in (("docker", "publish"), ("pages", "deploy"),
                          ("flatpak-pages", "publish"), ("snap-store", "promote"), ("winget", "submit")):
            with self.subTest(workflow=name):
                block = re.search(r"(?ms)^  " + job + r":\n(.*?)(?=^  [\w-]+:|\Z)", self.workflow(name))
                self.assertIsNotNone(block, "retain an explicit quarantined publisher job")
                self.assertRegex(block[1], r"(?m)^    if: \$\{\{ false && github.event_name == 'workflow_dispatch' \}\}$")
                gate = self.workflow("release-gate")
                self.assertNotRegex(gate, r"needs:.*\b" + re.escape(name) + r"\b")

    def test_package_failures_cannot_be_allowed_or_nightly_filtered(self):
        text = self.workflow("release")
        self.assertNotIn("continue-on-error", text)
        self.assertNotIn("packaging/nightly.sh", text)
        self.assertNotIn("needs.version.outputs.build", text)
        self.assertNotRegex(text, r"(?m)^  release:")
        # Keep actual platform packages and safe, nonpublishing checks.
        for job in ("linux", "flatpak", "arch", "snap", "freebsd", "macos-build", "macos", "windows", "web"):
            self.assertRegex(text, r"(?m)^  " + job + r":")
        self.assertIn("packaging/docker/smoke-test.sh", self.workflow("docker"))
        self.assertIn("cargo xtask web-offline-test --site dist/web", self.workflow("pages"))

    def test_package_smokes_do_not_start_network_enabled_desktop(self):
        text = self.workflow("release")
        self.assertNotIn("python3 packaging/linux/snap-smoke.py", text)
        self.assertIn("solidcraft.cli --version", text)
        # Flatpak's existing sandbox denies network; keep its real desktop smoke.
        manifest = (ROOT / "packaging/linux/flatpak/io.tangencylabs.solidcraft.bundle.yml").read_text()
        self.assertNotRegex(manifest, r"(?m)^\s+- --share=network\s*$")
        self.assertIn("packaging/linux/flatpak-bundle.sh", text)

    def test_build_only_tokens_and_fork_integration_lints_are_read_only(self):
        for name in ("packaging-lint", "lists"):
            text = self.workflow(name)
            self.assertRegex(text, r"branches: \[main, develop\]")
            self.assertNotRegex(text, r"(?m)^\s+[\w-]+: write\s*$")
        self.assertIn("scripts/ci/test_gate.py", self.workflow("packaging-lint"))


if __name__ == "__main__":
    unittest.main()
