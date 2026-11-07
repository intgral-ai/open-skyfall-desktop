#!/usr/bin/env python3
"""Counted execution tests; fixtures describe formats, not actual native runs.

Format facts: nexte.st/docs/machine-readable/list/ and /junit/, plus the
repository's xtask/src/known_failing.rs. No Node/runtime adapter is fabricated.
Run: PYTHONDONTWRITEBYTECODE=1 python3 -I scripts/ci/test_gate.py
"""
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
sys.path.insert(0, str(HERE))
import plan as planner  # noqa: E402

SHA = "98866161a5381f270b11eaee43344a7dba801d54"
PASS = "solidcraft-sketch::tests::passes"
FAIL = "solidcraft-sketch::tests::fails"
COMMON = ["-p", "solidcraft-sketch", "--cargo-profile", "ci", "--locked",
          "--profile", "ci", "--partition", "hash:1/2", "-E", "test(/^tests::/)"]
META = {"workspace_root": "/fixture", "packages": [
    {"name": "solidcraft-sketch", "manifest_path": "/fixture/crates/sketch/Cargo.toml", "dependencies": []}]}
ABSENT = dict(contractPresent=False, runtimePresent=False, runtimePayloadRequested=False,
              artifactVerifierPresent=False, missingContractInputs=sorted(planner.CONTRACT_INPUTS),
              missingRuntimeInputs=sorted((*planner.RUNTIME_INPUTS, ".node-version", "package-lock.json")))


def make_plan(paths=None, *, release=False):
    return planner.make_plan(META, paths if paths is not None else ["crates/sketch/src/lib.rs"],
                             ABSENT, source_revision=SHA, event_name="pull_request", base_ref="develop",
                             head_ref="task", full=False, release_required=release, diff_complete=True)


def plan_bytes(value):
    # Task 1's actual serialization, not a serializer supplied by the gate.
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode()


def listing():
    # Original representative data following TestListSummary (not a count-only stub).
    return {"rust-build-meta": {
        "target-directory": "/fixture/target", "base-output-directories": ["ci"],
        "non-test-binaries": {}, "build-script-out-dirs": {}, "linked-paths": [],
        "platforms": {"host": {"platform": {"triple": "x86_64-unknown-linux-gnu",
            "target-features": ["sse", "sse2"]}, "libdir": {"status": "available",
            "path": "/toolchain/lib"}}, "targets": []},
        "target-platforms": [{"triple": "x86_64-unknown-linux-gnu", "target-features": ["sse", "sse2"]}],
        "target-platform": None}, "test-count": 5, "rust-suites": {
        "solidcraft-sketch": {"package-name": "solidcraft-sketch", "binary-id": "solidcraft-sketch",
            "binary-name": "solidcraft-sketch", "package-id": "path+file:///fixture/crates/sketch#0.1.0",
            "kind": "lib", "binary-path": "/fixture/target/ci/deps/solidcraft_sketch-1234",
            "build-platform": "target", "cwd": "/fixture/crates/sketch", "status": "listed",
            "testcases": {
                "tests::passes": {"ignored": False, "filter-match": {"status": "matches"}},
                "tests::fails": {"ignored": False, "filter-match": {"status": "matches"}},
                "tests::ignored": {"ignored": True, "filter-match": {"status": "mismatch", "reason": "ignored"}},
                "tests::elsewhere": {"ignored": False, "filter-match": {"status": "mismatch", "reason": "partition"}},
                "other": {"ignored": False, "filter-match": {"status": "mismatch", "reason": "expression"}}}}}}


REPORT = '''<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="nextest-run" tests="2" failures="1" errors="0" time="0.02">
  <testsuite name="solidcraft-sketch" tests="2" failures="1" errors="0">
    <testcase name="tests::passes" classname="solidcraft-sketch" time="0.01"/>
    <testcase name="tests::fails" classname="solidcraft-sketch" time="0.01">
      <failure type="test failure with exit code 101">assertion failed</failure>
      <rerunFailure type="test failure"><system-out>failed again</system-out></rerunFailure>
    </testcase>
  </testsuite>
</testsuites>'''


def commands(junit, *, status=100, grader=True):
    result = [{"argv": ["cargo", "nextest", "list", *COMMON, "--message-format", "json"], "exitCode": 0},
              {"argv": ["cargo", "nextest", "run", *COMMON], "exitCode": status}]
    if grader:
        result.append({"argv": ["cargo", "xtask", "known-failing", "--junit", str(junit)], "exitCode": 0})
    return result


def command_receipt(value, job):
    return dict(version=1, sourceRevision=SHA, jobId=job, planDigest=hashlib.sha256(plan_bytes(value)).hexdigest(),
                commands=[{"argv": ["cargo", "xtask", "wasm" if job == "wasm" else "repo"], "exitCode": 0}], tests=None)


def native_receipt(value):
    result = command_receipt(value, "rust")
    result.update(commands=commands(Path("junit.xml")), tests=dict(expectedIds=[FAIL, PASS], passedIds=[PASS],
                  failedIds=[FAIL], skippedIds=[], knownFailingIds=[FAIL]))
    return result


class ExecutionEvidence(unittest.TestCase):
    def setUp(self):
        spec = importlib.util.find_spec("gate")
        if spec is None:
            self.fail("Task 2 gate.py is missing: counted execution is not implemented")
        self.gate = __import__("gate")
        self.tmp = tempfile.TemporaryDirectory(prefix=".ci-gate-test-", dir=ROOT)
        self.addCleanup(self.tmp.cleanup)
        self.directory = Path(self.tmp.name)
        self.selection = self.write("selection.json", listing())
        self.junit = self.write("junit.xml", REPORT)
        self.value = make_plan()
        self.needs = {job: "success" for job, entry in self.value["jobs"].items() if entry["decision"] == "run"}
        self.receipts = {job: command_receipt(self.value, job) for job in self.needs}
        self.receipts["rust"] = native_receipt(self.value)

    def write(self, name, value):
        path = self.directory / name
        path.write_text(value if isinstance(value, str) else json.dumps(value), encoding="utf-8")
        return path

    def evaluate(self):
        return self.gate.evaluate(self.value, self.needs, self.receipts)

    def rejected(self, fragment):
        problems = self.evaluate()
        self.assertTrue(problems, "incomplete execution must not satisfy the aggregate")
        self.assertIn(fragment.lower(), "\n".join(problems).lower())

    def record(self, *, cmds=None, known=None, job="rust"):
        return self.gate.record_receipt(plan_bytes(self.value), job,
            commands(self.junit) if cmds is None else cmds, self.selection, self.junit,
            {FAIL} if known is None else known)

    def cli(self, *args):
        return subprocess.run([sys.executable, "-I", str(HERE / "gate.py"), *map(str, args)],
                              cwd=ROOT, capture_output=True, text=True)

    def test_complete_native_known_failure_set_satisfies_regression_gate(self):
        self.assertEqual(self.evaluate(), [])

    def test_zero_expected_tests_is_not_success(self):
        self.receipts["rust"]["tests"] = {key: [] for key in self.receipts["rust"]["tests"]}
        self.rejected("expected")

    def test_skipped_only_is_not_execution(self):
        self.receipts["rust"]["tests"].update(passedIds=[], failedIds=[], skippedIds=[FAIL, PASS])
        self.rejected("skipped")

    def test_expected_test_absent_from_report_fails(self):
        self.receipts["rust"]["tests"]["passedIds"] = []
        self.rejected("missing")

    def test_unexpected_outcome_cannot_replace_expected_test(self):
        self.receipts["rust"]["tests"]["passedIds"] = ["unselected::test"]
        self.rejected("unexpected")

    def test_selected_job_requires_receipt(self):
        del self.receipts["rust"]
        self.rejected("receipt")

    def test_successful_receipt_does_not_override_real_job_result(self):
        for status in (None, "cancelled", "skipped", "failure", "pending"):
            with self.subTest(status=status):
                self.needs["rust"] = status
                self.rejected("job result")

    def test_wrong_source_revision_fails(self):
        self.receipts["rust"]["sourceRevision"] = "0" * 40
        self.rejected("sourceRevision")

    def test_wrong_plan_digest_fails(self):
        self.receipts["rust"]["planDigest"] = "0" * 64
        self.rejected("planDigest")

    def test_wrong_job_identity_fails(self):
        self.receipts["rust"]["jobId"] = "wasm"
        self.rejected("jobId")

    def test_duplicate_test_ids_fail_even_when_outcomes_agree(self):
        for key in ("expectedIds", "passedIds", "failedIds", "knownFailingIds"):
            with self.subTest(key=key):
                original = self.receipts["rust"]["tests"][key][:]
                self.receipts["rust"]["tests"][key] += original
                self.rejected("duplicate")
                self.receipts["rust"]["tests"][key] = original

    def test_conflicting_outcomes_fail(self):
        self.receipts["rust"]["tests"]["passedIds"].insert(0, FAIL)
        self.rejected("conflict")

    def test_unknown_raw_failure_fails(self):
        self.receipts["rust"]["tests"]["knownFailingIds"] = []
        self.rejected("REGRESSION")

    def test_listed_test_passing_requires_allowlist_removal(self):
        self.receipts["rust"]["tests"].update(passedIds=[FAIL, PASS], failedIds=[])
        self.receipts["rust"]["commands"][1]["exitCode"] = 0
        self.rejected("FIXED")

    def test_listed_test_not_selected_is_neither_fixed_nor_failed(self):
        self.receipts["rust"]["tests"]["knownFailingIds"].append("unselected::known")
        self.assertEqual(self.evaluate(), [])

    def test_infrastructure_build_and_filter_exits_are_not_known_failures(self):
        for status in (1, 2, 4, 101, 102, 130, -9):
            with self.subTest(status=status):
                self.receipts["rust"]["commands"][1]["exitCode"] = status
                self.rejected("exit")

    def test_nextest_100_requires_successful_reviewed_grader(self):
        self.receipts["rust"]["commands"].pop()
        self.rejected("grader")

    def test_failed_grader_cannot_be_ignored(self):
        self.receipts["rust"]["commands"][-1]["exitCode"] = 1
        self.rejected("exit")

    def test_exit_and_raw_failure_outcomes_must_agree(self):
        self.receipts["rust"]["commands"][1]["exitCode"] = 0
        self.rejected("exit")

    def test_release_missing_runtime_and_verifier_stays_blocked(self):
        self.value = make_plan(release=True)
        self.receipts = {}
        self.needs = {}
        problems = "\n".join(self.evaluate())
        self.assertIn("runtime: blocked", problems)
        self.assertIn("artifact-policy: blocked", problems)
        self.assertIn("packages: blocked", problems)

    def test_deleted_or_partial_runtime_cannot_be_waived_by_aggregate(self):
        for path in ("packages/mastra-runtime/src/main.ts", ".node-version"):
            with self.subTest(path=path):
                self.value = make_plan([path])
                self.assertIn("runtime: blocked", "\n".join(self.evaluate()))

    def test_release_cannot_claim_runtime_is_not_applicable(self):
        runtime = self.value["jobs"]["runtime"]
        self.value = make_plan(release=True)
        self.value["jobs"]["runtime"] = runtime
        self.value["openObligations"] = ["runtime"]
        self.rejected("runtime")

    def test_missing_consumer_decision_is_not_a_skip(self):
        del self.value["jobs"]["runtime"]
        self.rejected("runtime")

    def test_malformed_and_partial_receipts_fail_closed(self):
        original = copy.deepcopy(self.receipts["rust"])
        for receipt in (None, {}, {**original, "version": 2}, {**original, "commands": []},
                        {**original, "tests": {"expectedIds": [PASS]}},
                        {**original, "commands": [{"argv": ["true"], "exitCode": False}]}):
            with self.subTest(receipt=receipt):
                self.receipts["rust"] = receipt
                self.assertTrue(self.evaluate())

    def test_malformed_plan_values_return_problems_not_an_exception(self):
        for key, value in (("reason", []), ("decision", {}), ("missingInputs", "missing")):
            plan = make_plan()
            plan["jobs"]["runtime"][key] = value
            with self.subTest(key=key):
                self.assertTrue(self.gate.evaluate(plan, {}, {}))

    def test_every_future_test_bearing_consumer_rejects_command_only_evidence(self):
        # This is a hypothetical complete inventory, not a claim that #7 exists.
        inventory = dict(contractPresent=True, runtimePresent=True, runtimePayloadRequested=True,
                         artifactVerifierPresent=True, missingContractInputs=[], missingRuntimeInputs=[])
        self.value = planner.make_plan(META, ["packages/mastra-runtime/src/main.ts"], inventory,
            source_revision=SHA, event_name="pull_request", base_ref="develop", head_ref="task",
            full=True, release_required=True, diff_complete=True)
        for job in ("contract-rust", "runtime", "contract-ts", "integration", "packages"):
            with self.subTest(job=job):
                self.receipts = {job: command_receipt(self.value, job)}
                self.needs = {job: "success"}
                self.assertIn(job + ": test-bearing", "\n".join(self.evaluate()))

    def test_wasm_command_only_receipt_is_distinct_from_test_jobs(self):
        self.assertIsNone(self.receipts["wasm"]["tests"])
        self.assertEqual(self.evaluate(), [])
        self.receipts["rust"]["tests"] = None
        self.rejected("test-bearing")

    def test_command_only_receipt_needs_successful_nonempty_commands(self):
        for cmds in ([], [{"argv": ["cargo", "xtask", "wasm"], "exitCode": 100}]):
            with self.subTest(commands=cmds):
                self.receipts["wasm"]["commands"] = cmds
                self.rejected("wasm")

    def test_command_only_cannot_masquerade_as_counted_tests(self):
        self.receipts["wasm"]["tests"] = self.receipts["rust"]["tests"]
        self.rejected("command-only")

    def test_nextest_parser_uses_exact_binary_names_and_filter_matches(self):
        self.assertEqual(self.gate.read_nextest_selection(self.selection), {PASS, FAIL})

    def test_ignored_test_explicitly_selected_is_expected(self):
        data = listing()
        data["rust-suites"]["solidcraft-sketch"]["testcases"]["tests::ignored"]["filter-match"] = {"status": "matches"}
        self.assertEqual(self.gate.read_nextest_selection(self.write("ignored.json", data)),
                         {PASS, FAIL, "solidcraft-sketch::tests::ignored"})

    def test_binary_only_empty_malformed_and_partial_listings_fail(self):
        for data in ({}, {"rust-binaries": {}}, {"test-count": 0, "rust-build-meta": {}, "rust-suites": {}},
                     {"test-count": 5, "rust-build-meta": {}, "rust-suites": listing()["rust-suites"]}):
            with self.subTest(data=data):
                with self.assertRaises(ValueError):
                    self.gate.read_nextest_selection(self.write("bad.json", data))

    def test_listing_rejects_unknown_match_status_and_binary_id_conflict(self):
        for mutation in ("match", "binary", "count"):
            data = listing()
            suite = data["rust-suites"]["solidcraft-sketch"]
            if mutation == "match":
                suite["testcases"]["tests::passes"]["filter-match"]["status"] = "maybe"
            elif mutation == "binary":
                suite["binary-id"] = "different"
            else:
                data["test-count"] = 1
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                self.gate.read_nextest_selection(self.write("bad.json", data))

    def test_duplicate_json_keys_are_not_silently_deduplicated(self):
        text = self.selection.read_text().replace('"tests::passes":', '"tests::fails":', 1)
        with self.assertRaisesRegex(ValueError, "duplicate"):
            self.gate.read_nextest_selection(self.write("duplicate.json", text))

    def test_junit_parser_preserves_raw_ids_and_failure_error_skip_retry_semantics(self):
        xml = '''<testsuites name="nextest-run"><testsuite name="solidcraft-cli::action_matrix">
          <testcase classname="irrelevant" name="a_&lt;b&gt;"/>
          <testcase name="fails"><failure/><rerunFailure/></testcase>
          <testcase name="timeout"><error type="test timeout"/></testcase>
          <testcase name="ignored"><skipped/></testcase>
          <testcase name="flaky"><flakyFailure/><flakyError/></testcase>
        </testsuite></testsuites>'''
        self.assertEqual(self.gate.read_junit(self.write("outcomes.xml", xml)), {
            "solidcraft-cli::action_matrix::a_<b>": "passed", "solidcraft-cli::action_matrix::fails": "failed",
            "solidcraft-cli::action_matrix::timeout": "failed", "solidcraft-cli::action_matrix::ignored": "skipped",
            "solidcraft-cli::action_matrix::flaky": "passed"})

    def test_junit_rejects_empty_malformed_and_cases_outside_suite(self):
        for xml in ("", "<testsuites/>", "<testsuites><testcase name='x'/></testsuites>",
                    "<testsuites><testsuite name='a'><testcase/></testsuite></testsuites>",
                    "<testsuite name='a'><testcase name='x'></testsuite>"):
            with self.subTest(xml=xml), self.assertRaises(ValueError):
                self.gate.read_junit(self.write("invalid.xml", xml))

    def test_junit_duplicate_or_conflicting_cases_are_rejected(self):
        for second in ("", "<failure/>"):
            xml = "<testsuite name='a'><testcase name='x'/><testcase name='x'>" + second + "</testcase></testsuite>"
            with self.subTest(second=second), self.assertRaisesRegex(ValueError, "duplicate|conflict"):
                self.gate.read_junit(self.write("duplicate.xml", xml))

    def test_record_derives_sorted_outcomes_and_binds_exact_plan_bytes(self):
        receipt = self.record()
        self.assertEqual(receipt["tests"], dict(expectedIds=[FAIL, PASS], passedIds=[PASS], failedIds=[FAIL],
                                               skippedIds=[], knownFailingIds=[FAIL]))
        self.assertEqual(receipt["sourceRevision"], SHA)
        self.assertEqual(receipt["jobId"], "rust")
        self.assertEqual(receipt["planDigest"], hashlib.sha256(plan_bytes(self.value)).hexdigest())
        self.assertEqual(receipt["commands"][1]["exitCode"], 100)

    def test_record_rejects_partial_or_skip_only_junit(self):
        for xml in ("<testsuite name='solidcraft-sketch'><testcase name='tests::passes'/></testsuite>",
                    "<testsuite name='solidcraft-sketch'><testcase name='tests::passes'><skipped/></testcase>"
                    "<testcase name='tests::fails'><skipped/></testcase></testsuite>"):
            self.junit.write_text(xml)
            with self.subTest(xml=xml), self.assertRaises(ValueError):
                self.record()

    def test_record_rejects_unknown_failure_and_fixed_known_test(self):
        with self.assertRaisesRegex(ValueError, "REGRESSION"):
            self.record(known=set())
        with self.assertRaisesRegex(ValueError, "FIXED"):
            self.record(known={FAIL, PASS})

    def test_record_rejects_listing_failure_and_non_test_exit(self):
        for index, status in ((0, 100), (1, 101), (2, 1)):
            cmds = commands(self.junit)
            cmds[index]["exitCode"] = status
            with self.subTest(index=index, status=status), self.assertRaises(ValueError):
                self.record(cmds=cmds)

    def test_record_requires_same_package_filter_profile_and_partition_as_run(self):
        for flag, replacement in (("-p", "other"), ("-E", "all()"), ("--profile", "default"),
                                  ("--partition", "hash:2/2"), ("--cargo-profile", "release")):
            cmds = commands(self.junit)
            args = cmds[1]["argv"]
            args[args.index(flag) + 1] = replacement
            with self.subTest(flag=flag), self.assertRaisesRegex(ValueError, "configuration"):
                self.record(cmds=cmds)

    def test_listing_must_precede_execution_and_grading_follow_it(self):
        cmds = commands(self.junit)
        for reordered in ([cmds[1], cmds[0], cmds[2]], [cmds[0], cmds[2], cmds[1]]):
            with self.subTest(commands=reordered), self.assertRaises(ValueError):
                self.record(cmds=reordered)

    def test_no_tests_pass_escape_is_rejected(self):
        cmds = commands(self.junit)
        cmds[1]["argv"].append("--no-tests=pass")
        with self.assertRaises(ValueError):
            self.record(cmds=cmds)

    def test_no_tests_pass_separate_arguments_is_not_a_command_only_escape(self):
        self.receipts["wasm"]["commands"][0]["argv"] += ["--no-tests", "pass"]
        self.rejected("no-tests")

    def test_successful_native_run_has_no_known_failure_label(self):
        receipt = self.receipts["rust"]
        receipt["tests"].update(passedIds=[FAIL, PASS], failedIds=[], knownFailingIds=[])
        receipt["commands"][1]["exitCode"] = 0
        self.assertEqual(self.evaluate(), [])
        result = self.verify_files()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("rust: 2 selected tests passed", result.stdout)
        self.assertNotIn("with known failures", result.stdout)

    def test_grader_cannot_grade_a_different_report(self):
        cmds = commands(Path("another.xml"))
        with self.assertRaisesRegex(ValueError, "(?i)junit"):
            self.record(cmds=cmds)

    def test_record_wasm_needs_no_reports_but_native_does(self):
        receipt = self.gate.record_receipt(plan_bytes(self.value), "wasm",
            [{"argv": ["cargo", "xtask", "wasm"], "exitCode": 0}], None, None, set())
        self.assertIsNone(receipt["tests"])
        with self.assertRaisesRegex(ValueError, "test-bearing"):
            self.gate.record_receipt(plan_bytes(self.value), "rust", commands(self.junit), None, None, set())

    def test_record_refuses_non_applicable_or_blocked_job(self):
        for value in (self.value, make_plan(release=True)):
            with self.subTest(value=value), self.assertRaises(ValueError):
                self.gate.record_receipt(plan_bytes(value), "runtime", [], None, None, set())

    def verify_files(self, *, raw=None):
        plan = self.directory / "plan.json"
        plan.write_bytes(plan_bytes(self.value) if raw is None else raw)
        needs = self.write("needs.json", self.needs)
        receipt_dir = self.directory / "receipts"
        receipt_dir.mkdir(exist_ok=True)
        for job, receipt in self.receipts.items():
            (receipt_dir / (job + ".json")).write_text(json.dumps(receipt))
        return self.cli("verify", "--plan", plan, "--needs", needs, "--receipts", receipt_dir)

    def test_cli_host_foundation_prints_non_applicability_and_open_obligations(self):
        result = self.verify_files()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("runtime: not_applicable", result.stdout)
        self.assertIn("runtime_not_implemented_host_foundation", result.stdout)
        self.assertIn("open obligations:", result.stdout)
        self.assertIn("contract-ts", result.stdout)
        self.assertIn("regression gate satisfied with known failures", result.stdout)
        self.assertNotIn("all tests passed", result.stdout.lower())

    def test_cli_noncanonical_uploaded_plan_binds_exact_bytes_not_reserialization(self):
        raw = json.dumps(self.value, separators=(",", ":")).encode()
        digest = hashlib.sha256(raw).hexdigest()
        for receipt in self.receipts.values():
            receipt["planDigest"] = digest
        result = self.verify_files(raw=raw)
        self.assertEqual(result.returncode, 0, result.stderr)
        result = self.verify_files(raw=raw + b"\n")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("planDigest", result.stdout + result.stderr)

    def test_cli_record_uses_real_parsers_and_command_only_is_not_a_count_flag(self):
        plan = self.write("plan.json", plan_bytes(self.value).decode())
        cmds = self.write("commands.json", [{"argv": ["cargo", "xtask", "wasm"], "exitCode": 0}])
        output = self.directory / "receipt.json"
        result = self.cli("record", "--plan", plan, "--job-id", "wasm", "--commands", cmds, "--out", output)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIsNone(json.loads(output.read_text())["tests"])
        result = self.cli("record", "--plan", plan, "--job-id", "rust", "--commands", cmds, "--out", output)
        self.assertNotEqual(result.returncode, 0)

    def test_cli_duplicate_receipts_fail(self):
        result = self.verify_files()
        self.assertEqual(result.returncode, 0, result.stderr)
        directory = self.directory / "receipts"
        (directory / "copy.json").write_text(json.dumps(self.receipts["rust"]))
        result = self.verify_files()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("duplicate", result.stderr.lower())

    def test_cli_unknown_receipt_job_fails(self):
        self.receipts["unknown"] = command_receipt(self.value, "unknown")
        result = self.verify_files()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unexpected receipt", result.stdout + result.stderr)

    def test_cli_native_record_uses_current_allowlist_without_changing_raw_ids(self):
        # Use an existing reviewed identity as a format fixture, never execute it.
        identity = "solidcraft-cli::action_matrix::action_matrix_sample"
        data = listing()
        suite = data["rust-suites"].pop("solidcraft-sketch")
        binary = "solidcraft-cli::action_matrix"
        suite.update(**{"binary-id": binary, "binary-name": "action_matrix", "package-name": "solidcraft-cli",
                        "package-id": "path+file:///fixture/apps/solidcraft-cli#0.1.0",
                        "binary-path": "/fixture/target/ci/deps/action_matrix-1234",
                        "cwd": "/fixture/apps/solidcraft-cli", "kind": "test", "testcases": {
                            "action_matrix_sample": {"ignored": False, "filter-match": {"status": "matches"}}}})
        data["test-count"] = 1
        data["rust-suites"][binary] = suite
        self.write("selection.json", data)
        self.junit.write_text("<testsuite name='solidcraft-cli::action_matrix'>"
                              "<testcase name='action_matrix_sample'><failure/></testcase></testsuite>")
        value = planner.make_plan({"workspace_root": "/fixture", "packages": [{"name": "solidcraft-cli",
            "manifest_path": "/fixture/apps/solidcraft-cli/Cargo.toml", "dependencies": []}]},
            ["apps/solidcraft-cli/src/main.rs"], ABSENT, source_revision=SHA, event_name="pull_request",
            base_ref="develop", head_ref="task", full=False, release_required=False, diff_complete=True)
        path = self.write("plan.json", plan_bytes(value).decode())
        cmds = commands(self.junit)
        for cmd in cmds[:2]:
            cmd["argv"][cmd["argv"].index("-p") + 1] = "solidcraft-cli"
            cmd["argv"][cmd["argv"].index("-E") + 1] = "binary(action_matrix)"
        args = ["record", "--plan", path, "--job-id", "rust", "--commands", self.write("commands.json", cmds),
                "--selection", self.selection, "--junit", self.junit, "--out", self.directory / "native.json"]
        result = self.cli(*args)
        self.assertEqual(result.returncode, 0, result.stderr)
        receipt = json.loads((self.directory / "native.json").read_text())
        self.assertEqual(receipt["tests"]["failedIds"], [identity])
        self.assertIn(identity, receipt["tests"]["knownFailingIds"])
        fake = self.write("fake-known", identity + " unreviewed\n")
        result = self.cli(*args, "--known-failing", fake)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("reviewed", result.stderr)


class ReleaseWorkflowEvidence(unittest.TestCase):
    """Run the workflow's real aggregate against absent/host-only/stale evidence.

    No fixture is an actual runtime, installed test or successful verifier output.
    A green nested wrapper must never waive those missing consumers.
    """

    def text(self):
        return (ROOT / ".github/workflows/release-gate.yml").read_text()

    def test_direct_and_reusable_concurrency_keys_cannot_cancel_each_other(self):
        # Evaluate the actual emitted group keys, including workflow_call defaults.
        # Keep dedup within a path/ref, rather than making every SHA a new group.
        for workflow in ("ci", "kernel-rig"):
            text = (ROOT / ".github/workflows" / (workflow + ".yml")).read_text()
            group = re.search(r"(?m)^  group: (.+)$", text)[1]
            call = text.split("  workflow_call:", 1)[1].split("\npermissions:", 1)[0]
            default = re.search(r"(?m)^      concurrency_namespace:\n        type: string\n        default: (\S+)", call)

            def key(caller, reusable, ref="refs/pull/42/merge"):
                values = {"github.workflow": caller, "github.event_name": "pull_request", "github.ref": ref,
                          "inputs.concurrency_namespace || 'direct'": default[1] if reusable and default else "direct"}
                return re.sub(r"\$\{\{\s*(.*?)\s*\}\}", lambda m: values[m[1]], group)

            with self.subTest(workflow=workflow):
                direct = key("CI" if workflow == "ci" else "Kernel rig", False)
                release = key("Release gate", True)
                self.assertNotEqual(direct, release, "a main PR's direct and release-required calls must not share cancellation")
                self.assertEqual(release, key("Release gate", True), "updates within the same caller/ref must still dedup")
                self.assertNotEqual(release, key("Native baseline", True), "independent reusable callers must not cancel")
                self.assertNotEqual(release, key("Release gate", True, "refs/pull/43/merge"))
                self.assertIn("cancel-in-progress: ${{ github.event_name == 'pull_request' }}", text)

    def script(self):
        match = re.search(r"(?ms)^          python3 - <<'PY'\n(.*?)^          PY$", self.text())
        self.assertIsNotNone(match, "release-gate needs a source-bound execution/evidence aggregate")
        return "\n".join(line[10:] for line in match[1].splitlines()) + "\n"

    def run_gate(self, *, stale=False, host_only=False, statuses=None):
        with tempfile.TemporaryDirectory(prefix=".release-gate-test-", dir=ROOT) as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            subprocess.run(["git", "-C", str(root), "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                            "commit", "-q", "--allow-empty", "-m", "final tested source"], check=True)
            sha = subprocess.check_output(["git", "-C", str(root), "rev-parse", "HEAD"], text=True).strip()
            (root / "scripts").mkdir()
            (root / "scripts/ci").symlink_to(HERE, target_is_directory=True)
            value = planner.make_plan(META, [], ABSENT, source_revision=sha,
                event_name="pull_request", base_ref="main", head_ref="release/v1.2.3",
                full=True, release_required=True, diff_complete=True)
            (root / "selection").mkdir()
            (root / "selection/plan.json").write_bytes(plan_bytes(value))
            (root / "evidence").mkdir()
            package = root / "package-evidence/release-linux-x86_64"
            package.mkdir(parents=True)
            (package / "build.json").write_text(json.dumps(dict(packageId="release-linux-x86_64", sourceRevision="0" * 40 if stale else sha,
                target="x86_64-unknown-linux-gnu", contractDigest=None, artifacts=[])))
            if host_only:
                (package / "runtime-artifact.json").write_text(json.dumps(dict(mode="host_only", status="not_applicable")))
            env = dict(os.environ, GITHUB_SHA=sha, NEEDS_JSON=json.dumps({job: {"result": status} for job, status in
                (statuses or dict(ci="success", packages="success", **{"kernel-rig": "success"})).items()}))
            return subprocess.run([sys.executable, "-c", self.script()], cwd=root, env=env,
                                  capture_output=True, text=True)

    def test_main_pr_merge_group_and_manual_reusable_have_an_always_top_level_gate(self):
        text = self.text()
        self.assertRegex(text, r"(?m)^  pull_request:\n(?:    .*\n)*    branches: \[main\]")
        self.assertRegex(text, r"(?m)^  merge_group:")
        self.assertRegex(text, r"(?m)^  workflow_call:")
        self.assertRegex(text, r"(?m)^  workflow_dispatch:")
        gate = text.split("\n  release-gate:\n", 1)[1]
        self.assertIn("name: release-gate", gate)
        self.assertIn("if: always()", gate)
        self.assertIn("${{ toJSON(needs) }}", gate)
        self.assertIn("validation_mode: full", text)
        self.assertIn("release_required: true", text)

    def test_existing_rig_is_a_full_same_run_consumer_not_a_permanent_failure_stub(self):
        result = self.run_gate()
        self.assertIn("kernel-rig handoff:", result.stdout + result.stderr)
        self.assertNotIn("kernel-rig: full source-bound PR/manual evidence adapter not integrated", result.stdout)
        self.assertIn("full_validation: true", self.text())
        self.assertIn("kernel-rig-handoff-${{ github.sha }}", self.text())

    def test_green_wrappers_without_runtime_contract_inventory_installed_evidence_fail(self):
        result = self.run_gate()
        self.assertNotEqual(result.returncode, 0)
        for missing in ("runtime: blocked", "contract-rust: blocked", "#15", "#14"):
            self.assertIn(missing, result.stdout + result.stderr)

    def test_host_only_report_cannot_satisfy_runtime_required_release(self):
        result = self.run_gate(host_only=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("artifact", result.stdout + result.stderr)
        self.assertIn("#14", result.stdout + result.stderr)

    def test_pre_promotion_source_descriptor_is_not_retagged_to_final_sha(self):
        result = self.run_gate(stale=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("sourceRevision mismatch", result.stdout + result.stderr)

    def test_skipped_cancelled_failed_or_missing_required_workflow_cannot_pass(self):
        for status in ("skipped", "cancelled", "failure", "missing"):
            with self.subTest(status=status):
                result = self.run_gate(statuses=dict(ci="success", packages=status, **{"kernel-rig": "success"}))
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("packages: real job result", result.stdout + result.stderr)

    def test_package_descriptor_records_actual_bytes_checkout_target_and_contract(self):
        text = (ROOT / ".github/workflows/release.yml").read_text()
        match = re.search(r"(?ms)^        shell: python \{0\}\n        run: \|\n(.*?)^      -", text)
        self.assertIsNotNone(match, "package producers need actual checkout/byte metadata")
        script = "\n".join(line[10:] for line in match[1].splitlines()) + "\n"
        with tempfile.TemporaryDirectory(prefix=".package-metadata-test-", dir=ROOT) as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            subprocess.run(["git", "-C", str(root), "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                            "commit", "-q", "--allow-empty", "-m", "build source"], check=True)
            sha = subprocess.check_output(["git", "-C", str(root), "rev-parse", "HEAD"], text=True).strip()
            target = root / "dist/release"
            target.mkdir(parents=True)
            env = dict(os.environ, GITHUB_SHA=sha, PACKAGE_ID="release-linux-x86_64", PACKAGE_TARGET="x86_64-unknown-linux-gnu")
            command = [sys.executable, "-c", script]
            result = subprocess.run(command, cwd=root, env=env, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0, "empty packages must not get a descriptor")
            self.assertFalse((target / "build.json").exists())
            (target / "fixture.tar.gz").write_bytes(b"abc")
            result = subprocess.run(command, cwd=root, env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            data = json.loads((target / "build.json").read_text())
            self.assertEqual(data["sourceRevision"], sha)
            self.assertEqual(data["target"], "x86_64-unknown-linux-gnu")
            self.assertIsNone(data["contractDigest"], "absent #5 is never a fabricated digest")
            self.assertEqual(data["artifacts"], [dict(name="fixture.tar.gz",
                sha256="ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")])
            manifest = root / "schema/mastra-runtime/v1/manifest.json"
            manifest.parent.mkdir(parents=True)
            manifest.write_bytes(b"{}\n")
            result = subprocess.run(command, cwd=root, env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads((target / "build.json").read_text())["contractDigest"],
                             "sha256:" + hashlib.sha256(b"{}\n").hexdigest())
            result = subprocess.run(command, cwd=root, env={**env, "GITHUB_SHA": "0" * 40}, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("sourceRevision mismatch", result.stderr)
            subprocess.run(["git", "-C", str(root), "add", "schema/mastra-runtime/v1/manifest.json"], check=True)
            subprocess.run(["git", "-C", str(root), "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                            "commit", "-q", "-m", "tracked contract fixture"], check=True)
            env["GITHUB_SHA"] = subprocess.check_output(["git", "-C", str(root), "rev-parse", "HEAD"], text=True).strip()
            manifest.write_bytes(b'{"changed":true}\n')
            result = subprocess.run(command, cwd=root, env=env, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0, "modified tracked bytes cannot be attributed to clean HEAD")
            self.assertIn("dirty", result.stderr)

    def test_intermediate_native_artifact_handoffs_are_bound_to_final_checkout(self):
        text = (ROOT / ".github/workflows/release.yml").read_text()
        self.assertIn("ref: ${{ github.sha }}", text)
        # These are native build inputs, not a staged runtime or installed test.
        self.assertEqual(text.count("name: icons-${{ github.sha }}"), 2)
        self.assertIn("name: macos-bin-${{ matrix.arch }}-${{ github.sha }}", text)
        self.assertIn("name: macos-bin-aarch64-${{ github.sha }}", text)
        self.assertIn("name: macos-bin-x86_64-${{ github.sha }}", text)

    def test_every_actual_package_build_result_is_required_without_allowing_skips(self):
        text = (ROOT / ".github/workflows/release.yml").read_text().split("\n  package-builds:\n", 1)[1]
        match = re.search(r"(?ms)^          python3 - <<'PY'\n(.*?)^          PY$", text)
        self.assertIsNotNone(match)
        script = "\n".join(line[10:] for line in match[1].splitlines()) + "\n"
        for job in ("version", "icons", "linux", "flatpak", "arch", "snap", "freebsd", "macos-build", "macos", "windows", "web"):
            for status in ("failure", "cancelled", "skipped", "missing"):
                with self.subTest(job=job, status=status):
                    # Even otherwise successful wrappers cannot waive one build.
                    needs = {name: {"result": "success"} for name in
                        ("version", "icons", "linux", "flatpak", "arch", "snap", "freebsd", "macos-build", "macos", "windows", "web")}
                    if status == "missing":
                        del needs[job]
                    else:
                        needs[job]["result"] = status
                    result = subprocess.run([sys.executable, "-c", script], env={**os.environ, "NEEDS_JSON": json.dumps(needs)},
                                            capture_output=True, text=True)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("FAIL: " + job + ":", result.stdout)


if __name__ == "__main__":
    unittest.main()
