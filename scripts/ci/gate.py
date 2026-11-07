#!/usr/bin/env python3
"""Count execution, not green wrappers. Receipts are evidence from trusted CI jobs.

Before running tests, capture `cargo nextest list --message-format json` with
exactly the run's package, build, profile, filter and partition arguments. Record
that command first, the run second, and (for rust) the existing known-failing
grader last. Keep their actual argv/exitCode, listing and JUnit; never supply
counts or replace raw failures. This module does not execute consumers or create
a future runtime adapter. Test-bearing adapters must supply real listing/JUnit.

evaluate() accepts Task 1's dict (its deterministic JSON serialization). The CLI
retains the exact uploaded bytes, including whitespace, for digest verification.
Receipt/job success is not artifact provenance verification or publication approval.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import sys
import xml.etree.ElementTree as ET

TEST_JOBS = frozenset(("rust", "contract-rust", "runtime", "contract-ts", "integration", "packages"))
COMMAND_JOBS = frozenset(("repo", "pr-policy", "wasm", "artifact-policy"))
JOBS = TEST_JOBS | COMMAND_JOBS
ROOT = Path(__file__).resolve().parents[2]
KNOWN_FAILING = ROOT / "tests/KNOWN_FAILING_TESTS"
ID_FIELDS = ("expectedIds", "passedIds", "failedIds", "skippedIds", "knownFailingIds")
FOUNDATION_REASONS = {
    "contract-rust": {"contract_not_implemented_host_foundation"},
    "runtime": {"runtime_not_implemented_host_foundation"},
    "contract-ts": {"runtime_not_implemented_host_foundation", "contract_not_implemented_host_foundation"},
    "integration": {"runtime_not_implemented_host_foundation", "contract_not_implemented_host_foundation"},
    "artifact-policy": {"verifier_not_implemented_host_foundation"},
}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def nonempty(value):
    return isinstance(value, str) and bool(value.strip())


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, f"duplicate JSON key: {key}")
        result[key] = value
    return result


def parse_json(data):
    return json.loads(data, object_pairs_hook=unique_object)


def read_nextest_selection(path: Path) -> set[str]:
    """Read full TestListSummary, not binaries-only metadata or a caller's count.

    test-count includes ignored/filtered cases. Only filter-match=matches is
    expected; ignored=True is still expected under --run-ignored all/only.
    """
    data = parse_json(Path(path).read_bytes())
    require(isinstance(data, dict), "not a nextest test listing")
    meta, suites, count = data.get("rust-build-meta"), data.get("rust-suites"), data.get("test-count")
    require(isinstance(meta, dict) and nonempty(meta.get("target-directory")), "missing nextest rust-build-meta")
    require(isinstance(suites, dict) and type(count) is int and count >= 0, "missing nextest rust-suites/test-count")
    selected, all_ids = set(), set()
    for binary, suite in suites.items():
        require(nonempty(binary) and isinstance(suite, dict), "invalid nextest suite")
        require(suite.get("binary-id") == binary, f"conflicting binary-id: {binary}")
        # Suites skipped before listing have no test identities; they cannot supply evidence.
        status = suite.get("status", "listed")  # older nextest omits status
        cases = suite.get("testcases")
        require(status in ("listed", "skipped") and isinstance(cases, dict), f"invalid suite status/testcases: {binary}")
        require(status == "listed" or not cases, f"skipped suite contains testcases: {binary}")
        for name, case in cases.items():
            require(nonempty(name) and isinstance(case, dict) and type(case.get("ignored")) is bool,
                    f"invalid nextest testcase: {binary}::{name}")
            match = case.get("filter-match")
            require(isinstance(match, dict) and match.get("status") in ("matches", "mismatch"),
                    f"invalid filter-match: {binary}::{name}")
            require(match["status"] != "mismatch" or nonempty(match.get("reason")), "missing mismatch reason")
            identity = f"{binary}::{name}"
            require(identity not in all_ids, f"duplicate test ID: {identity}")
            all_ids.add(identity)
            if match["status"] == "matches":
                selected.add(identity)
    require(count == len(all_ids), "nextest test-count disagrees with listed testcases")
    require(selected, "expected test set is empty")
    return selected


def read_junit(path: Path) -> dict[str, str]:
    """Use the existing grader's suite::case identities and retry semantics.

    failure/error are raw failures, flakyFailure/flakyError a final pass. Unlike
    the grader, retain skipped outcomes and reject duplicate/conflicting cases.
    """
    raw = Path(path).read_bytes()
    require(b"<!DOCTYPE" not in raw.upper() and b"<!ENTITY" not in raw.upper(), "JUnit declarations are not allowed")
    try:
        root = ET.fromstring(raw)
    except ET.ParseError as error:
        raise ValueError(f"not a JUnit report: {error}") from error
    require(root.tag in ("testsuites", "testsuite"), "not a JUnit report root")
    outcomes = {}

    def visit(element, suite=None):
        if element.tag == "testsuite":
            require(nonempty(element.get("name")), "JUnit testsuite has no name")
            suite = element.get("name")
        if element.tag == "testcase":
            require(suite is not None, "JUnit testcase outside a testsuite")
            require(nonempty(element.get("name")), "JUnit testcase has no name")
            identity = f"{suite}::{element.get('name')}"
            require(identity not in outcomes, f"duplicate/conflicting JUnit ID: {identity}")
            tags = {child.tag for child in element}
            failed, skipped = bool(tags & {"failure", "error"}), "skipped" in tags
            require(not (failed and skipped), f"conflicting JUnit outcomes: {identity}")
            require(not any(child.tag in ("testcase", "testsuite") for child in element), "nested JUnit testcase")
            outcomes[identity] = "skipped" if skipped else "failed" if failed else "passed"
            return
        for child in element:
            visit(child, suite)

    visit(root)
    require(outcomes, "JUnit contains no test outcomes")
    return outcomes


def ids(value, name):
    require(isinstance(value, list) and all(nonempty(v) and "::" in v for v in value), f"invalid {name}")
    require(len(value) == len(set(value)), f"duplicate IDs in {name}")
    require(value == sorted(value), f"unsorted IDs in {name}")
    return set(value)


def validate_plan(plan):
    require(isinstance(plan, dict) and type(plan.get("version")) is int and plan["version"] == 1, "invalid plan version")
    require(isinstance(plan.get("sourceRevision"), str) and re.fullmatch(r"[0-9a-f]{40}", plan["sourceRevision"]),
            "invalid plan sourceRevision")
    require(type(plan.get("releaseRequired")) is bool, "invalid releaseRequired")
    jobs = plan.get("jobs")
    require(isinstance(jobs, dict), "missing jobs")
    require(set(jobs) == JOBS, f"missing/unknown job decisions: {', '.join(sorted(JOBS ^ set(jobs)))}")
    obligations = plan.get("openObligations")
    require(isinstance(obligations, list) and all(isinstance(j, str) and j in JOBS for j in obligations)
            and obligations == sorted(set(obligations)), "invalid openObligations")
    expected_obligations = []
    for job, entry in jobs.items():
        require(isinstance(entry, dict), f"{job}: invalid decision")
        decision, reason, missing = entry.get("decision"), entry.get("reason"), entry.get("missingInputs")
        require(decision in ("run", "blocked", "not_applicable"), f"{job}: invalid decision")
        require(nonempty(reason), f"{job}: invalid applicability reason")
        require(isinstance(missing, list) and all(nonempty(p) for p in missing)
                and missing == sorted(set(missing)), f"{job}: invalid missingInputs")
        if decision == "blocked":
            require(reason == "missing_required_consumer" and missing, f"{job}: invalid blocked consumer")
        else:
            require(not missing, f"{job}: missing inputs cannot be waived")
            allowed = {"no_relevant_change"} | FOUNDATION_REASONS.get(job, set()) if decision == "not_applicable" else {
                "always", "selected_change", "full_validation", "release_required"}
            require(reason in allowed, f"{job}: invalid applicability reason")
            if decision == "not_applicable" and reason != "no_relevant_change":
                expected_obligations.append(job)
        if plan["releaseRequired"] and job not in ("repo", "pr-policy"):
            require(decision != "not_applicable", f"{job}: required release consumer cannot be not_applicable")
    require(sorted(expected_obligations) == obligations, "openObligations do not match non-applicability")
    require(jobs["repo"]["decision"] == "run", "repo must always run")
    pr = plan.get("eventName") in ("pull_request", "pull_request_target", "merge_group")
    require(jobs["pr-policy"]["decision"] == ("run" if pr else "not_applicable"), "pr-policy applicability mismatch")


class BoundPlan(dict):
    """Keep exact transport bytes without changing Task 1's JSON fields."""
    def __init__(self, raw):
        value = parse_json(raw)
        validate_plan(value)
        super().__init__(value)
        self.raw = raw


def digest_for(plan):
    if isinstance(plan, BoundPlan):
        require(parse_json(plan.raw) == plan, "plan changed after loading")
        raw = plan.raw
    else:
        # make_plan's public dict uses plan.py's deterministic output format.
        raw = (json.dumps(plan, indent=2, sort_keys=True) + "\n").encode()
    return hashlib.sha256(raw).hexdigest()


def nextest_configuration(argv, verb):
    """Conservatively compare every argument except documented output/run controls.

    Do not reimplement nextest's filters or partition hash. Unknown differences
    fail closed rather than guessing equivalent package/filter configuration.
    """
    require(argv[:3] == ["cargo", "nextest", verb], f"missing nextest {verb} command")
    result, index, json_listing = [], 3, False
    value_controls = {"--test-threads", "-j", "--retries", "--status-level", "--final-status-level",
                      "--failure-output", "--success-output"} if verb == "run" else set()
    flags = {"--no-fail-fast", "--fail-fast", "--no-capture", "--nocapture"} if verb == "run" else set()
    while index < len(argv):
        arg = argv[index]
        key, equal, value = arg.partition("=")
        require(not (key == "--no-tests" and (value == "pass" or
                    (not equal and index + 1 < len(argv) and argv[index + 1] == "pass"))), "--no-tests=pass is forbidden")
        if verb == "list" and key in ("--message-format", "-T"):
            if not equal:
                index += 1
                require(index < len(argv), "missing listing message format")
                value = argv[index]
            require(value in ("json", "json-pretty") and not json_listing, "listing must use JSON once")
            json_listing = True
        elif key in value_controls:
            if not equal:
                index += 1
                require(index < len(argv), f"missing {key} value")
        elif arg not in flags:
            result.append(arg)
        index += 1
    require(verb != "list" or json_listing, "listing must use JSON")
    return result


def validate_commands(commands, job, tests, junit=None):
    require(isinstance(commands, list) and commands, "commands must be nonempty")
    for command in commands:
        require(isinstance(command, dict) and isinstance(command.get("argv"), list)
                and command["argv"] and all(nonempty(a) for a in command["argv"]), "invalid command argv")
        require(type(command.get("exitCode")) is int, "invalid command exitCode")
        argv = command["argv"]
        require(not any(arg == "--no-tests=pass" or
                        (arg == "--no-tests" and argv[i + 1:i + 2] == ["pass"])
                        for i, arg in enumerate(argv)), "--no-tests=pass is forbidden")
    if job in COMMAND_JOBS:
        require(tests is None, "command-only job must have tests=null")
        require(all(c["exitCode"] == 0 for c in commands), "command exit is nonzero")
        return
    require(isinstance(tests, dict), "test-bearing job requires test evidence")
    listings = [i for i, c in enumerate(commands) if c["argv"][:3] == ["cargo", "nextest", "list"]]
    runs = [i for i, c in enumerate(commands) if c["argv"][:3] == ["cargo", "nextest", "run"]]
    require(len(listings) == len(runs) == 1, "test-bearing job needs one actual nextest listing/run pair; no future adapter is assumed")
    before, run = listings[0], runs[0]
    require(before < run, "nextest selection must precede execution")
    require(nextest_configuration(commands[before]["argv"], "list") == nextest_configuration(commands[run]["argv"], "run"),
            "listing/run configuration mismatch (package/filter/profile/partition)")
    status = commands[run]["exitCode"]
    require(status in ((0, 100) if job == "rust" else (0,)), f"nextest infrastructure/unapproved exit: {status}")
    require(bool(tests["failedIds"]) == (status == 100), "nextest exit disagrees with raw failure outcomes")
    require(all(c["exitCode"] == 0 for i, c in enumerate(commands) if i != run), "command/list/grader exit is nonzero")
    if job == "rust":
        graders = [i for i, c in enumerate(commands) if c["argv"][:3] == ["cargo", "xtask", "known-failing"]]
        require(len(graders) == 1 and graders[0] > run, "successful reviewed known-failing grader must follow run")
        argv = commands[graders[0]]["argv"]
        require(len(argv) == 5 and argv[3] == "--junit", "grader must use the JUnit report and default reviewed list")
        require(junit is None or Path(argv[4]).resolve() == Path(junit).resolve(), "grader JUnit report mismatch")


def validate_tests(tests, job):
    if job in COMMAND_JOBS:
        require(tests is None, "command-only job must have tests=null")
        return
    require(isinstance(tests, dict), "test-bearing job requires test evidence")
    sets = {key: ids(tests.get(key), key) for key in ID_FIELDS}
    expected, passed, failed, skipped, known = (sets[key] for key in ID_FIELDS)
    require(expected, "expected test set is empty")
    require(not (passed & failed or passed & skipped or failed & skipped), "conflicting test outcomes")
    require(not skipped, "skipped tests are not executed evidence: " + ", ".join(sorted(skipped)))
    actual = passed | failed | skipped
    require(not actual - expected, "unexpected test outcomes: " + ", ".join(sorted(actual - expected)))
    require(not expected - actual, "missing expected outcomes: " + ", ".join(sorted(expected - actual)))
    require(not failed - known, "REGRESSION, failed and not on known-failing list: " + ", ".join(sorted(failed - known)))
    require(not passed & known, "FIXED, remove from known-failing list: " + ", ".join(sorted(passed & known)))
    require(job == "rust" or not failed, "known failures are accepted only in the native reviewed regression gate")


def validate_receipt(receipt, plan, job, *, junit=None):
    require(isinstance(receipt, dict) and type(receipt.get("version")) is int and receipt["version"] == 1, "invalid receipt version")
    require(receipt.get("sourceRevision") == plan["sourceRevision"], "receipt sourceRevision mismatch")
    require(receipt.get("planDigest") == digest_for(plan), "receipt planDigest mismatch")
    require(receipt.get("jobId") == job, "receipt jobId mismatch")
    require("tests" in receipt, "receipt missing tests field")
    validate_tests(receipt["tests"], job)
    validate_commands(receipt.get("commands"), job, receipt["tests"], junit)


def record_receipt(plan_bytes: bytes, job_id: str, commands: list[dict], selection: Path | None,
                   junit: Path | None, known_failures: set[str]) -> dict:
    plan = BoundPlan(plan_bytes)
    require(job_id in JOBS and plan["jobs"][job_id]["decision"] == "run", f"{job_id}: job is not selected to run")
    tests = None
    if job_id in TEST_JOBS:
        require(selection is not None and junit is not None, "test-bearing job requires selection and JUnit reports")
        expected, outcomes = read_nextest_selection(selection), read_junit(junit)
        require(isinstance(known_failures, set) and all(nonempty(k) for k in known_failures), "invalid known failures")
        tests = dict(expectedIds=sorted(expected), knownFailingIds=sorted(known_failures),
                     **{key: sorted(k for k, outcome in outcomes.items() if outcome == status)
                        for key, status in (("passedIds", "passed"), ("failedIds", "failed"), ("skippedIds", "skipped"))})
    else:
        require(selection is None and junit is None, "command-only job does not accept test reports")
    receipt = dict(version=1, sourceRevision=plan["sourceRevision"], jobId=job_id, planDigest=digest_for(plan),
                   commands=commands, tests=tests)
    validate_receipt(receipt, plan, job_id, junit=junit)
    return receipt


def evaluate(plan: dict, needs: dict[str, str], receipts: dict[str, dict]) -> list[str]:
    """Empty problems means selected evidence is sufficient, not repository-wide success."""
    try:
        validate_plan(plan)
        require(isinstance(needs, dict) and isinstance(receipts, dict), "needs/receipts must be objects")
    except ValueError as error:
        return [str(error)]
    problems = []
    for job in receipts:
        if job not in JOBS or plan["jobs"][job]["decision"] != "run":
            problems.append(f"{job}: unexpected receipt for unselected job")
    for job, entry in sorted(plan["jobs"].items()):
        if entry["decision"] == "blocked":
            problems.append(f"{job}: blocked ({entry['reason']}): {', '.join(entry['missingInputs'])}")
        elif entry["decision"] == "run":
            if needs.get(job) != "success":
                problems.append(f"{job}: real job result is not success: {needs.get(job, 'missing')}")
            if job not in receipts:
                problems.append(f"{job}: missing receipt")
                continue
            try:
                validate_receipt(receipts[job], plan, job)
            except ValueError as error:
                problems.append(f"{job}: {error}")
    return problems


def read_known_failures(path):
    require(Path(path).resolve() == KNOWN_FAILING, "use the repository's reviewed tests/KNOWN_FAILING_TESTS")
    previous, result = None, set()
    for line in KNOWN_FAILING.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split(None, 1)
        require(len(parts) == 2 and parts[1].strip(), "known-failing entry gives no cause")
        identity = parts[0]
        require(previous is None or previous < identity, "known-failing IDs duplicate/out of order")
        result.add(identity)
        previous = identity
    return result


def print_summary(plan, receipts, problems):
    for job, entry in sorted(plan["jobs"].items()):
        if entry["decision"] == "not_applicable":
            print(f"{job}: not_applicable ({entry['reason']})")
        elif not problems and job in receipts:
            tests = receipts[job]["tests"]
            if tests is None:
                print(f"{job}: command-only evidence satisfied")
            elif tests["failedIds"]:
                print(f"{job}: regression gate satisfied with known failures; {len(tests['passedIds'])} passed, "
                      f"{len(tests['failedIds'])} known failing; raw failures: {', '.join(tests['failedIds'])}")
            else:
                print(f"{job}: {len(tests['passedIds'])} selected tests passed")
    print("open obligations: " + (", ".join(plan["openObligations"]) or "none"))
    for problem in problems:
        print("FAIL: " + problem)
    if not problems:
        print("selected execution evidence sufficient (not a repository-wide test claim)")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_subparsers(dest="mode", required=True)
    record = modes.add_parser("record")
    verify = modes.add_parser("verify")
    for mode in (record, verify):
        mode.add_argument("--plan", type=Path, required=True)
    for flag in ("job-id", "commands", "out"):
        record.add_argument("--" + flag, required=True)
    for flag in ("selection", "junit"):
        record.add_argument("--" + flag, type=Path)
    record.add_argument("--known-failing", type=Path, default=KNOWN_FAILING)
    verify.add_argument("--needs", type=Path, required=True)
    verify.add_argument("--receipts", type=Path, required=True)
    args = parser.parse_args()
    try:
        raw = args.plan.read_bytes()
        if args.mode == "record":
            commands = parse_json(Path(args.commands).read_bytes())
            known = read_known_failures(args.known_failing) if args.job_id == "rust" else set()
            receipt = record_receipt(raw, args.job_id, commands, args.selection, args.junit, known)
            Path(args.out).write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        else:
            plan, receipts = BoundPlan(raw), {}
            require(args.receipts.is_dir(), "missing receipts directory")
            for file in sorted(args.receipts.rglob("*.json")):
                receipt = parse_json(file.read_bytes())
                require(isinstance(receipt, dict) and isinstance(receipt.get("jobId"), str), f"invalid receipt: {file}")
                job = receipt["jobId"]
                require(job not in receipts, f"duplicate receipt jobId: {job}")
                receipts[job] = receipt
            problems = evaluate(plan, parse_json(args.needs.read_bytes()), receipts)
            print_summary(plan, receipts, problems)
            if problems:
                return 1
    except (OSError, ValueError, TypeError) as error:
        print(f"gate: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
