#!/usr/bin/env python3
"""Self-test for main_merge.py. Run: python3 -I scripts/ci/test_main_merge.py (from the repo root).

Builds small throwaway git repositories; needs only git.
"""
import os
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import hashlib
import contextlib
import io

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import main_merge  # noqa: E402


class Repo:
    def __init__(self, root):
        self.root = root
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "t@example.com")
        self.git("config", "user.name", "t")
        self.git("config", "commit.gpgsign", "false")

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.root, capture_output=True, text=True, check=True).stdout.strip()

    def write(self, path, text):
        with open(os.path.join(self.root, path), "w") as f:
            f.write(text)

    def commit(self, msg, **files):
        for path, text in files.items():
            self.write(path, text)
        self.git("add", "-A")
        self.git("commit", "-q", "-m", msg)
        return self.git("rev-parse", "HEAD")


LINES = "".join(f"line {i}\n" for i in range(40))


class SamePullRequest(unittest.TestCase):
    def test_listed_pull_request_must_match(self):
        self.assertTrue(main_merge.same_pull_request({"pull_requests": [{"number": 7}]}, 7, "x/y", "b"))
        self.assertFalse(main_merge.same_pull_request({"pull_requests": [{"number": 8}]}, 7, "x/y", "b"))

    def test_fork_run_matches_by_head_repository_and_branch(self):
        run = {"pull_requests": [], "head_repository": {"full_name": "fork/solidcraft"}, "head_branch": "fix"}
        self.assertTrue(main_merge.same_pull_request(run, 7, "fork/solidcraft", "fix"))
        self.assertFalse(main_merge.same_pull_request(run, 7, "fork/solidcraft", "other"))
        self.assertFalse(main_merge.same_pull_request(run, 7, "someone/solidcraft", "fix"))


class ForkExecution(unittest.TestCase):
    """A green wrapper or a main-only historical run must not authorize this SHA."""

    def adapter(self, name):
        self.assertTrue(hasattr(main_merge, name), f"missing fork CI adapter: {name}")
        return getattr(main_merge, name)

    def test_legacy_cli_never_carries_a_prior_result(self):
        script = Path(main_merge.__file__).resolve()
        result = subprocess.run([sys.executable, "-I", str(script), "--base", "missing", "--head", "missing",
                                 "--repo", "fork/repo", "--pr", "1", "--head-repo", "fork/repo", "--head-ref", "task"],
                                capture_output=True, text=True, env={**os.environ, "GH_TOKEN": "not-a-real-token"})
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "carried=\n")
        self.assertEqual(result.stderr, "", "retired carry CLI must not inspect git or the network")

    def test_pr_and_full_use_the_same_listing_and_run_selection(self):
        build = self.adapter("native_commands")
        for full in (False, True):
            with self.subTest(full=full):
                commands = build({"rustFull": False, "rustPackages": ["base", "top"]}, full=full)
                listing, run, grader = commands
                self.assertEqual(listing[:3], ["cargo", "nextest", "list"])
                self.assertEqual(run[:3], ["cargo", "nextest", "run"])
                self.assertIn("--locked", listing)
                self.assertIn("--locked", run)
                self.assertEqual(listing[3:-2], run[3:-2])
                self.assertEqual(listing[-2:], ["--message-format", "json"])
                self.assertEqual(run[-2:], ["--test-threads", "2"])
                expression = run[run.index("-E") + 1]
                self.assertEqual(expression, "all()" if full else
                                 "default() & not (binary(action_matrix) & test(=action_matrix_sample))")
                self.assertEqual(grader, ["cargo", "xtask", "known-failing", "--junit", "target/nextest/ci/junit.xml"])

    def test_empty_rust_package_scope_is_an_error_not_workspace_fallback(self):
        build = self.adapter("native_commands")
        with self.assertRaises(ValueError):
            build({"rustFull": False, "rustPackages": []}, full=False)

    def test_command_capture_preserves_actual_argv_and_failure_exit(self):
        run = self.adapter("capture_commands")
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory)
            argv = [sys.executable, "-I", "-c", "import sys; print('actual output'); sys.exit(7)"]
            commands = run([argv], out)
            self.assertEqual(commands, [{"argv": argv, "exitCode": 7}])
            self.assertEqual(json.loads((out / "commands.json").read_text()), commands)
            self.assertIn("actual output", (out / "command-0.log").read_text())

    def test_real_needs_are_flattened_without_turning_skips_into_success(self):
        flatten = self.adapter("required_results")
        self.assertEqual(flatten({"rust": {"result": "skipped"}, "repo": {"result": "failure"},
                                  "wasm": {"result": "cancelled"}}),
                         {"rust": "skipped", "repo": "failure", "wasm": "cancelled"})

    def test_non_pr_and_integration_choose_full_without_label_input(self):
        full = self.adapter("full_validation")
        for event, mode, release, expected in [
            ("pull_request", "affected", False, False), ("merge_group", "affected", False, True),
            ("push", "affected", False, True), ("schedule", "affected", False, True),
            ("workflow_dispatch", "affected", False, False), ("pull_request", "full", False, True),
            ("pull_request", "affected", True, True),
        ]:
            self.assertEqual(full(event, mode, release), expected)
        with self.assertRaises(ValueError):
            full("pull_request", "quick", False)


class ForkWorkflowIntegration(unittest.TestCase):
    """External Cargo is a process fixture, never claimed as a product test run."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.repo = Repo(self.root)
        self.source = self.repo.commit("source", **{"README.md": "foundation\n"})
        previous = Path.cwd()
        self.addCleanup(os.chdir, previous)
        os.chdir(self.root)
        previous_env = os.environ.copy()
        self.addCleanup(lambda: (os.environ.clear(), os.environ.update(previous_env)))
        self.meta = {"workspace_root": str(self.root), "packages": [
            {"name": "base", "manifest_path": str(self.root / "crates/base/Cargo.toml"), "dependencies": []}]}
        self.bin = self.root / "bin"
        self.bin.mkdir()
        cargo = self.bin / "cargo"
        cargo.write_text(f"#!{sys.executable}\n" + '''import json, os, pathlib, sys
args = sys.argv[1:]
if args[0] == 'metadata':
    print(os.environ['TEST_METADATA'])
elif args[:2] == ['nextest', 'list']:
    cases = {} if os.environ.get('EMPTY_SELECTION') else {'tests::ok': {'ignored': False, 'filter-match': {'status': 'matches'}}}
    print(json.dumps({'rust-build-meta': {'target-directory': str(pathlib.Path('target').resolve())}, 'test-count': len(cases),
                      'rust-suites': {'base': {'binary-id': 'base', 'status': 'listed', 'testcases': cases}}}))
elif args[:2] == ['nextest', 'run']:
    report = pathlib.Path('target/nextest/ci/junit.xml'); report.parent.mkdir(parents=True, exist_ok=True)
    outcome = '<failure/>' if os.environ.get('TEST_EXIT') == '100' else ''
    report.write_text('<testsuites><testsuite name="base"><testcase name="tests::ok">'+outcome+'</testcase></testsuite></testsuites>')
    sys.exit(int(os.environ.get('TEST_EXIT', '0')))
elif args[:2] == ['xtask', 'known-failing']:
    sys.exit(int(os.environ.get('GRADER_EXIT', '0')))
else:
    sys.exit(97)
''')
        cargo.chmod(0o755)
        os.environ.update(PATH=str(self.bin) + os.pathsep + os.environ['PATH'], TEST_METADATA=json.dumps(self.meta),
                          GITHUB_EVENT_NAME="pull_request", GITHUB_OUTPUT=str(self.root / "outputs"))
        self.plan_file = self.root / "plan.json"

    def plan(self, paths, *, release=False):
        value = main_merge.applicability.make_plan(self.meta, paths,
                    main_merge.applicability.detect_inventory(self.root, paths), source_revision=self.source,
                    event_name="pull_request", base_ref="develop", head_ref="task", full=False,
                    release_required=release, diff_complete=True)
        # Noncanonical bytes deliberately exercise exact artifact transport binding.
        self.plan_file.write_text(json.dumps(value) + "\n\n")
        return value

    def test_actual_native_listing_run_grader_yield_counted_bound_receipt(self):
        self.plan(["crates/base/src/lib.rs"])
        main_merge.execute_workflow(self.plan_file, "rust", self.root / "evidence/rust")
        receipt = json.loads((self.root / "evidence/rust/receipt.json").read_text())
        self.assertEqual(receipt["sourceRevision"], self.source)
        self.assertEqual(receipt["planDigest"], hashlib.sha256(self.plan_file.read_bytes()).hexdigest())
        self.assertEqual(receipt["tests"]["expectedIds"], ["base::tests::ok"])
        self.assertEqual(receipt["tests"]["passedIds"], ["base::tests::ok"])
        self.assertEqual([c["exitCode"] for c in receipt["commands"]], [0, 0, 0])
        self.assertTrue((self.root / "evidence/rust/runner.json").is_file())

    def test_empty_native_selection_cannot_make_a_receipt(self):
        self.plan(["crates/base/src/lib.rs"])
        os.environ["EMPTY_SELECTION"] = "1"
        with self.assertRaisesRegex(ValueError, "empty"):
            main_merge.execute_workflow(self.plan_file, "rust", self.root / "evidence/rust")
        self.assertFalse((self.root / "evidence/rust/receipt.json").exists())

    def test_infrastructure_exit_unknown_failure_and_failed_grader_remain_blocking(self):
        self.plan(["crates/base/src/lib.rs"])
        for test_exit, grader_exit in [(102, 0), (100, 0), (0, 1)]:
            with self.subTest(test_exit=test_exit, grader_exit=grader_exit):
                os.environ.update(TEST_EXIT=str(test_exit), GRADER_EXIT=str(grader_exit))
                out = self.root / f"evidence/{test_exit}-{grader_exit}"
                with self.assertRaises((ValueError, FileNotFoundError)):
                    main_merge.execute_workflow(self.plan_file, "rust", out)
                self.assertFalse((out / "receipt.json").exists())
                self.assertEqual(json.loads((out / "commands.json").read_text())[1]["exitCode"], test_exit)
                self.assertTrue((out / "junit.xml").exists(), "keep raw report even after failure")

    def test_wrong_checkout_rejected_before_commands(self):
        self.plan(["crates/base/src/lib.rs"])
        self.repo.commit("new source", **{"README.md": "new\n"})
        with self.assertRaisesRegex(ValueError, "checkout SHA"):
            main_merge.execute_workflow(self.plan_file, "rust", self.root / "evidence/rust")
        self.assertFalse((self.root / "evidence/rust/commands.json").exists())

    def test_present_but_unwired_consumer_cannot_get_a_success_stub(self):
        value = self.plan(["README.md"])
        value["jobs"]["contract-rust"] = {"decision": "run", "reason": "selected_change", "missingInputs": []}
        value["openObligations"].remove("contract-rust")
        self.plan_file.write_text(json.dumps(value))
        with self.assertRaisesRegex(ValueError, "handoff"):
            main_merge.execute_workflow(self.plan_file, "contract-rust", self.root / "evidence/contract")
        self.assertFalse((self.root / "evidence/contract/receipt.json").exists())

    def test_selection_uses_real_diff_including_both_names_and_actual_checkout(self):
        base = self.source
        self.repo.git("mv", "README.md", "unowned.txt")
        self.repo.git("commit", "-qm", "rename")
        head = self.repo.git("rev-parse", "HEAD")
        event = {"pull_request": {"base": {"sha": base, "ref": "develop"}, "head": {"sha": head, "ref": "task"}}}
        value = main_merge.select_workflow(event, "affected", False, self.plan_file)
        self.assertEqual(value["sourceRevision"], head)
        self.assertEqual(set((self.root / "paths.nul").read_bytes().split(b"\0")) - {b""}, {b"README.md", b"unowned.txt"})
        self.assertEqual(value["jobs"]["rust"]["decision"], "run")
        self.assertEqual(value["scope"], "full")
        outputs = dict(line.split("=", 1) for line in (self.root / "outputs").read_text().splitlines())
        self.assertEqual(json.loads(outputs["plan"]), value)

    def test_unavailable_diff_and_release_remain_conservative(self):
        event = {"pull_request": {"base": {"sha": "f" * 40, "ref": "develop"}, "head": {"sha": self.source, "ref": "task"}}}
        value = main_merge.select_workflow(event, "affected", False, self.plan_file)
        self.assertEqual(value["scope"], "full")
        self.assertEqual(value["jobs"]["wasm"]["decision"], "run")
        value = main_merge.select_workflow(event, "affected", True, self.plan_file)
        self.assertEqual(value["jobs"]["runtime"]["decision"], "blocked")
        self.assertEqual(value["jobs"]["packages"]["decision"], "blocked")

    def test_repository_command_inventory_preserves_all_existing_gates(self):
        scripts = self.root / "scripts/claim"
        scripts.mkdir(parents=True)
        (scripts / "fixture.test.js").write_text("// fixture\n")
        with self.assertRaisesRegex(ValueError, "packaging"):
            main_merge.repository_commands()
        tests = self.root / "packaging/tests"
        tests.mkdir(parents=True)
        for name in ("apprun", "flatpak", "nightly", "windows_icon"):
            (tests / (name + "_test.py")).write_text("# fixture entrypoint\n")
        commands = main_merge.repository_commands()
        for name in ("apprun", "flatpak", "nightly", "windows_icon"):
            self.assertTrue(any(c[-1] == "packaging/tests/" + name + "_test.py" for c in commands))
        for name in ("assets", "icons", "licences", "layers", "ui-containers", "lists", "i18n", "mesh-decisions", "skills", "web-offline-test"):
            self.assertTrue(any(c[:7] == ["cargo", "run", "--locked", "--package", "xtask", "--", name] for c in commands), name)
        self.assertIn(["python3", "-I", "scripts/ci/test_gate.py"], commands)
        self.assertIn(["bash", "scripts/test-format-pending.sh"], commands)
        self.assertEqual(commands[-1], ["node", "--test", "scripts/claim/fixture.test.js"])

    def test_packaging_owning_commands_fail_on_nonempty_file_with_zero_tests(self):
        (self.root / "scripts/claim").mkdir(parents=True)
        (self.root / "scripts/claim/fixture.test.js").write_text("// fixture\n")
        tests = self.root / "packaging/tests"
        tests.mkdir(parents=True)
        for name in ("apprun", "flatpak", "nightly", "windows_icon"):
            (tests / (name + "_test.py")).write_text("# no registered tests\n")
        commands = [c for c in main_merge.repository_commands() if c[-1].startswith("packaging/tests/")]
        self.assertEqual(len(commands), 4)
        for command in commands:
            result = subprocess.run(command, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0, "zero owning tests must not pass: " + str(command))

    def test_wasm_follows_real_layer_table_not_native_apps(self):
        path = self.root / "xtask/src/layers.rs"
        path.parent.mkdir(parents=True)
        path.write_text('("geom", Class::Layer(0)), ("derive", Class::Standalone), ("ui-egui", Class::Layer(5))')
        self.meta["packages"] = [{"name": n} for n in ("solidcraft-geom", "solidcraft-ui-egui", "solidcraft-derive", "solidcraft", "xtask")]
        os.environ["TEST_METADATA"] = json.dumps(self.meta)
        self.assertEqual(main_merge.wasm_commands(), [["cargo", "check", "--locked", "--target", "wasm32-unknown-unknown", "--profile", "ci",
                         "-p", "solidcraft-geom", "-p", "solidcraft-ui-egui", "-p", "solidcraft-web"]])

    def policy_event(self, base="develop", head="task", head_repo="fork/repo"):
        target = self.root / "scripts/pr-checks/pr-checks.js"
        target.parent.mkdir(parents=True, exist_ok=True)
        source = Path(main_merge.__file__).resolve().parents[2] / "scripts/pr-checks/pr-checks.js"
        target.write_bytes(source.read_bytes())
        event = self.root / "event.json"
        event.write_text(json.dumps({"pull_request": {"base": {"ref": base}, "head": {"ref": head, "repo": {"full_name": head_repo}}}}))
        os.environ.update(GITHUB_EVENT_PATH=str(event), GITHUB_REPOSITORY="fork/repo")

    def test_required_policy_runs_actual_branch_policy_not_an_advisory_success(self):
        self.plan(["README.md"])
        self.policy_event()
        main_merge.execute_workflow(self.plan_file, "pr-policy", self.root / "evidence/policy")
        receipt = json.loads((self.root / "evidence/policy/receipt.json").read_text())
        self.assertEqual(receipt["commands"][0]["exitCode"], 0)
        for base, head, repo in [("main", "task", "fork/repo"), ("main", "release/v1.2.3", "external/repo")]:
            with self.subTest(base=base, head=head, repo=repo):
                self.policy_event(base, head, repo)
                out = self.root / ("rejected/" + head)
                with self.assertRaisesRegex(ValueError, "nonzero"):
                    main_merge.execute_workflow(self.plan_file, "pr-policy", out)
                self.assertEqual(json.loads((out / "commands.json").read_text())[0]["exitCode"], 1)
                self.assertFalse((out / "receipt.json").exists())

    def foundation_evidence(self):
        self.plan(["README.md"])
        self.policy_event()
        main_merge.execute_workflow(self.plan_file, "pr-policy", self.root / "receipts/pr-policy")
        # The repo consumer is intentionally a small actual command fixture here;
        # repository_commands() separately protects the real production gate inventory.
        out = self.root / "receipts/repo"
        commands = main_merge.capture_commands([[sys.executable, "-I", "-c", "print('repository fixture')"]], out)
        receipt = main_merge.gate.record_receipt(self.plan_file.read_bytes(), "repo", commands, None, None, set())
        (out / "receipt.json").write_text(json.dumps(receipt))
        self.needs = {"changes": {"result": "success", "outputs": {"deps": "false"}},
                      "repo": {"result": "success"}, "pr-policy": {"result": "success"}}

    def aggregate(self):
        needs_file = self.root / "needs.json"
        needs_file.write_text(json.dumps(self.needs))
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            status = main_merge.aggregate_workflow(self.plan_file, needs_file, self.root / "receipts")
        return status, output.getvalue()

    def test_aggregate_accepts_explicit_host_na_but_not_skips_or_missing_receipts(self):
        self.foundation_evidence()
        status, output = self.aggregate()
        self.assertEqual(status, 0, output)
        self.assertIn("runtime_not_implemented_host_foundation", output)
        self.assertIn("open obligations:", output)
        for status in ("failure", "skipped", "cancelled"):
            with self.subTest(status=status):
                self.needs["repo"]["result"] = status
                self.assertEqual(self.aggregate()[0], 1)
        self.needs["repo"]["result"] = "success"
        (self.root / "receipts/repo/receipt.json").unlink()
        self.assertEqual(self.aggregate()[0], 1)

    def test_aggregate_rejects_changed_plan_bytes_and_blocked_release(self):
        self.foundation_evidence()
        self.plan_file.write_bytes(self.plan_file.read_bytes() + b"\n")
        status, output = self.aggregate()
        self.assertEqual(status, 1)
        self.assertIn("planDigest mismatch", output)
        self.plan(["README.md"], release=True)
        status, output = self.aggregate()
        self.assertEqual(status, 1)
        self.assertIn("runtime: blocked", output)
        self.assertIn("packages: blocked", output)

    def test_manual_cross_compile_cannot_be_ignored_by_ci(self):
        self.foundation_evidence()
        os.environ["REQUIRE_RISCV"] = "true"
        status, output = self.aggregate()
        self.assertEqual(status, 1)
        self.assertIn("riscv64-gates", output)

    def test_full_validation_requires_platform_heavy_and_random_property_jobs(self):
        self.foundation_evidence()
        value = main_merge.applicability.make_plan(self.meta, ["crates/base/src/lib.rs"],
                    main_merge.applicability.detect_inventory(self.root, []), source_revision=self.source,
                    event_name="pull_request", base_ref="develop", head_ref="task", full=True,
                    release_required=False, diff_complete=True)
        self.plan_file.write_text(json.dumps(value))
        main_merge.execute_workflow(self.plan_file, "rust", self.root / "receipts/rust")
        for job in ("repo", "wasm"):
            out = self.root / "receipts" / job
            commands = main_merge.capture_commands([[sys.executable, "-I", "-c", "pass"]], out)
            receipt = main_merge.gate.record_receipt(self.plan_file.read_bytes(), job, commands, None, None, set())
            (out / "receipt.json").write_text(json.dumps(receipt))
        main_merge.execute_workflow(self.plan_file, "pr-policy", self.root / "receipts/pr-policy")
        extras = ["rust", "wasm", "clippy", "doc-tests", "oracle", "windows-fxc", "windows-startup",
                  "property-tests", "build-tests", "action-matrix", "action-matrix-determinism", "dependency-advisories"]
        self.needs.update({job: {"result": "success"} for job in extras})
        self.assertEqual(self.aggregate()[0], 0)
        for job in extras:
            with self.subTest(job=job):
                self.needs[job]["result"] = "skipped"
                status, output = self.aggregate()
                self.assertEqual(status, 1)
                self.assertIn(job + ":", output)
                self.needs[job]["result"] = "success"

    def test_actual_cli_native_exit_and_empty_selection_failure(self):
        self.plan(["crates/base/src/lib.rs"])
        script = str(Path(main_merge.__file__).resolve())
        argv = [sys.executable, "-I", script, "execute", "--plan", str(self.plan_file), "--job", "rust", "--out"]
        result = subprocess.run([*argv, str(self.root / "cli-success")], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.root / "cli-success/receipt.json").exists())
        result = subprocess.run([*argv, str(self.root / "cli-empty")], capture_output=True, text=True,
                                env={**os.environ, "EMPTY_SELECTION": "1"})
        self.assertEqual(result.returncode, 1)
        self.assertIn("expected test set is empty", result.stderr)
        self.assertFalse((self.root / "cli-empty/receipt.json").exists())


class MergeOfMain(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.r = Repo(self.tmp.name)
        self.r.commit("base", **{"a.txt": LINES, "b.txt": LINES})
        self.r.git("checkout", "-q", "-b", "pr")
        self.change = self.r.commit("pr change", **{"a.txt": LINES.replace("line 5\n", "line five\n")})
        self.cwd = os.getcwd()
        os.chdir(self.tmp.name)

    def tearDown(self):
        os.chdir(self.cwd)
        self.tmp.cleanup()

    def main_commit(self, **files):
        self.r.git("checkout", "-q", "main")
        sha = self.r.commit("main moves", **files)
        self.r.git("checkout", "-q", "pr")
        return sha

    def merge_main(self):
        self.r.git("merge", "-q", "--no-edit", "main")
        return self.r.git("rev-parse", "HEAD")

    def test_clean_merge_of_unrelated_main_change_is_carried(self):
        base = self.main_commit(**{"b.txt": LINES.replace("line 30\n", "line thirty\n")})
        head = self.merge_main()
        self.assertEqual(main_merge.merge_of_main(base, head), self.change)

    def test_consecutive_merges_of_main_chain_back_to_the_change(self):
        self.main_commit(**{"b.txt": LINES.replace("line 30\n", "line thirty\n")})
        first = self.merge_main()
        base = self.main_commit(**{"b.txt": LINES.replace("line 30\n", "line thirty\n").replace("line 35\n", "line 35b\n")})
        head = self.merge_main()
        self.assertEqual(main_merge.merge_chain(base, head), [first, self.change])

    def test_chain_stops_at_an_ordinary_commit(self):
        self.main_commit(**{"b.txt": LINES.replace("line 30\n", "line thirty\n")})
        self.merge_main()
        later = self.r.commit("more work", **{"a.txt": LINES.replace("line 5\n", "line five\n").replace("line 20\n", "line twenty\n")})
        base = self.main_commit(**{"b.txt": LINES.replace("line 30\n", "line thirty\n").replace("line 35\n", "line 35b\n")})
        head = self.merge_main()
        self.assertEqual(main_merge.merge_chain(base, head), [later])

    def test_conflict_resolution_is_not_carried(self):
        base = self.main_commit(**{"a.txt": LINES.replace("line 5\n", "line 5 on main\n")})
        subprocess.run(["git", "merge", "-q", "main"], cwd=self.tmp.name, capture_output=True)
        self.r.write("a.txt", LINES.replace("line 5\n", "line five on both\n"))
        self.r.git("add", "-A")
        self.r.git("commit", "-q", "--no-edit")
        head = self.r.git("rev-parse", "HEAD")
        self.assertIsNone(main_merge.merge_of_main(base, head))

    def test_main_change_next_to_the_change_is_not_carried(self):
        # Main edits a context line of the pull request's hunk: the change's patch differs.
        base = self.main_commit(**{"a.txt": LINES.replace("line 7\n", "line seven\n")})
        head = self.merge_main()
        self.assertIsNone(main_merge.merge_of_main(base, head))

    def test_whitespace_change_in_the_change_is_not_carried(self):
        # A conflict resolution that only respaces the change's own line still changes the change.
        base = self.main_commit(**{"a.txt": LINES.replace("line 5\n", "line 5 on main\n")})
        subprocess.run(["git", "merge", "-q", "main"], cwd=self.tmp.name, capture_output=True)
        self.r.write("a.txt", LINES.replace("line 5\n", "line  five\n"))
        self.r.git("add", "-A")
        self.r.git("commit", "-q", "--no-edit")
        head = self.r.git("rev-parse", "HEAD")
        self.assertIsNone(main_merge.merge_of_main(base, head))

    def test_ordinary_commit_is_not_carried(self):
        head = self.r.commit("more", **{"a.txt": LINES.replace("line 5\n", "line 5b\n")})
        base = self.r.git("rev-parse", "main")
        self.assertIsNone(main_merge.merge_of_main(base, head))

    def test_merge_of_another_branch_is_not_carried(self):
        base = self.r.git("rev-parse", "main")
        self.r.git("checkout", "-q", "-b", "other", "main")
        self.r.commit("other work", **{"c.txt": "x\n"})
        self.r.git("checkout", "-q", "pr")
        self.r.git("merge", "-q", "--no-edit", "other")
        head = self.r.git("rev-parse", "HEAD")
        self.assertIsNone(main_merge.merge_of_main(base, head))

    def test_change_with_empty_diff_is_not_carried(self):
        self.r.git("checkout", "-q", "-b", "empty", "main")
        base = self.main_commit(**{"b.txt": LINES.replace("line 30\n", "line thirty\n")})
        self.r.git("checkout", "-q", "empty")
        self.r.git("merge", "-q", "--no-ff", "--no-edit", "main")
        head = self.r.git("rev-parse", "HEAD")
        self.assertIsNone(main_merge.merge_of_main(base, head))


if __name__ == "__main__":
    unittest.main()
