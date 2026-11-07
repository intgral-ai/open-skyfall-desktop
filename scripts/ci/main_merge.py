#!/usr/bin/env python3
"""Fork CI execution adapter; historical merge helpers remain tested below.

The historical carry CLI is retired: it always prints `carried=` without reading
git or the API. A main-specific historical result is not evidence for this SHA.
The select/execute/aggregate subcommands bind Task 1 and Task 2 to real commands.
"""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time
import urllib.request

sys.path.insert(0, str(Path(__file__).resolve().parent))
import affected  # noqa: E402
import gate  # noqa: E402
import plan as applicability  # noqa: E402

API = "https://api.github.com"
SHARD_PREFIX = "Native Linux tests"
# Consecutive merges of main followed back to a run whose shards ran.
MAX_CHAIN = 10


def git(*args):
    return subprocess.run(["git", *args], capture_output=True, text=True, check=True).stdout


def patch_id(a, b):
    """The stable patch id of `git diff a b` ("" for an empty diff)."""
    diff = subprocess.run(["git", "diff", "--no-renames", a, b], capture_output=True, check=True).stdout
    if not diff.strip():
        return ""
    # --verbatim: --stable ignores whitespace, which matters in strings and indentation.
    out = subprocess.run(["git", "patch-id", "--verbatim"], input=diff, capture_output=True, check=True).stdout
    return out.split()[0].decode() if out.split() else ""


def merge_of_main(base, head):
    """The first parent of HEAD when HEAD only merges main into an unchanged change, else None."""
    parents = git("rev-list", "--parents", "-n", "1", head).split()[1:]
    if len(parents) != 2:
        return None
    change, merged = parents
    if subprocess.run(["git", "merge-base", "--is-ancestor", merged, base]).returncode != 0:
        return None
    fork_point = git("merge-base", change, merged).strip()
    before, after = patch_id(fork_point, change), patch_id(merged, head)
    return change if before and before == after else None


def merge_chain(base, head):
    """The changes HEAD carries from, nearest first: the first parent of each consecutive merge of
    main that left the change's own patch as it was (each one equals the next, so all equal HEAD's)."""
    chain, cur = [], head
    for _ in range(MAX_CHAIN):
        change = merge_of_main(base, cur)
        if change is None:
            break
        chain.append(change)
        cur = change
    return chain


def get(url, token):
    req = urllib.request.Request(url, headers={"Accept": "application/vnd.github+json", "Authorization": f"Bearer {token}"})
    with urllib.request.urlopen(req, timeout=20) as r:
        return json.load(r)


def same_pull_request(run, pr, head_repo, head_ref):
    """Whether a workflow run belongs to this pull request, not another one with the same head."""
    numbers = [p.get("number") for p in run.get("pull_requests") or []]
    if numbers:
        return pr in numbers
    return (run.get("head_repository") or {}).get("full_name") == head_repo and run.get("head_branch") == head_ref


def passing_run(repo, sha, token, pr, head_repo, head_ref):
    """URL of a successful ci.yml run of this pull request at `sha` whose test shards all passed."""
    runs = get(f"{API}/repos/{repo}/actions/workflows/ci.yml/runs?head_sha={sha}&event=pull_request&status=success&per_page=10", token)
    for run in runs.get("workflow_runs", []):
        if not same_pull_request(run, pr, head_repo, head_ref):
            continue
        jobs = get(f"{API}/repos/{repo}/actions/runs/{run['id']}/jobs?per_page=100", token).get("jobs", [])
        shards = [j for j in jobs if j.get("name", "").startswith(SHARD_PREFIX)]
        if shards and all(j.get("conclusion") == "success" for j in shards):
            return run.get("html_url", "")
    return ""


def full_validation(event, mode, release_required):
    if mode not in ("affected", "full"):
        raise ValueError("validation_mode must be affected or full")
    return release_required or mode == "full" or event in ("push", "merge_group", "schedule")


def package_scope(plan):
    if plan["rustFull"]:
        return ["--workspace"]
    gate.require(plan["rustPackages"], "selected Rust package scope is empty")
    return [arg for package in plan["rustPackages"] for arg in ("-p", package)]


def native_commands(plan, *, full):
    expression = "all()" if full else "default() & not (binary(action_matrix) & test(=action_matrix_sample))"
    common = package_scope(plan) + ["--cargo-profile", "ci", "--locked", "--profile", "ci",
                                    "--ignore-default-filter", "-E", expression]
    return [["cargo", "nextest", "list", *common, "--message-format", "json"],
            ["cargo", "nextest", "run", *common, "--test-threads", "2"],
            ["cargo", "xtask", "known-failing", "--junit", "target/nextest/ci/junit.xml"]]


def capture_commands(argvs, out):
    """Capture real subprocess exit/argv; a log or failed command is never a receipt."""
    out = Path(out)
    out.mkdir(parents=True, exist_ok=True)
    runner = {key: os.environ.get(key) for key in ("ImageOS", "ImageVersion", "RUNNER_OS", "RUNNER_ARCH", "RUNNER_NAME")}
    (out / "runner.json").write_text(json.dumps(runner, indent=2) + "\n", encoding="utf-8")
    commands = []
    measurements = []
    for index, argv in enumerate(argvs):
        started, disk_before = time.monotonic(), shutil.disk_usage(".").free
        print("EXEC " + json.dumps(argv), flush=True)
        # Native listing stdout must remain exact JSON, not a merged diagnostic log.
        listing = argv[:3] == ["cargo", "nextest", "list"]
        target = out / ("selection.json" if listing else f"command-{index}.log")
        with target.open("wb") as stdout, (out / f"command-{index}.stderr").open("wb") as stderr:
            try:
                status = subprocess.run(argv, stdout=stdout, stderr=stderr, check=False).returncode
            except OSError as error:
                stderr.write(str(error).encode())
                status = 127
        commands.append(dict(argv=argv, exitCode=status))
        measurements.append(dict(commandIndex=index, elapsedSeconds=time.monotonic() - started,
                                 diskFreeBefore=disk_before, diskFreeAfter=shutil.disk_usage(".").free))
        (out / "resources.json").write_text(json.dumps(measurements, indent=2) + "\n", encoding="utf-8")
        (out / "commands.json").write_text(json.dumps(commands, indent=2) + "\n", encoding="utf-8")
        if not listing:
            print(target.read_text(encoding="utf-8", errors="replace"), flush=True)
        print((out / f"command-{index}.stderr").read_text(encoding="utf-8", errors="replace"), file=sys.stderr, flush=True)
        if status and not (argv[:3] == ["cargo", "nextest", "run"] and status == 100):
            break
    return commands


def required_results(needs):
    gate.require(isinstance(needs, dict), "Actions needs must be an object")
    return {job: value.get("result", "missing") for job, value in needs.items()}


def check_revision(plan):
    gate.require(git("rev-parse", "HEAD").strip() == plan["sourceRevision"], "actual checkout SHA differs from selection plan")


def select_workflow(event, mode, release_required, out):
    """Derive the only plan from this checkout, not labels or a success declaration."""
    name = os.environ["GITHUB_EVENT_NAME"]
    source = git("rev-parse", "HEAD").strip()
    pr, group = event.get("pull_request", {}), event.get("merge_group", {})
    base = pr.get("base", {}).get("sha") if name == "pull_request" else group.get("base_sha") if name == "merge_group" else event.get("before")
    head = pr.get("head", {}).get("sha") if name == "pull_request" else source
    base_ref = pr.get("base", {}).get("ref", "") if name == "pull_request" else group.get("base_ref", os.environ.get("GITHUB_REF_NAME", ""))
    head_ref = pr.get("head", {}).get("ref", "") if name == "pull_request" else group.get("head_ref", os.environ.get("GITHUB_REF_NAME", ""))
    complete, paths, raw = False, [], b""
    if base and head:
        diff = subprocess.run(["git", "diff", "--no-renames", "--name-only", "-z", f"{base}...{head}"], capture_output=True)
        complete = diff.returncode == 0
        if complete:
            raw = diff.stdout
            paths = [p.decode("utf-8") for p in raw.split(b"\0") if p]
    full = full_validation(name, mode, release_required)
    meta = affected.load_metadata(None)
    result = applicability.make_plan(meta, paths, applicability.detect_inventory(Path.cwd(), paths), source_revision=source,
                                    event_name=name, base_ref=base_ref, head_ref=head_ref, full=full,
                                    release_required=release_required, diff_complete=complete)
    out = Path(out)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    (out.parent / "paths.nul").write_bytes(raw)
    native = result["jobs"]["rust"]["decision"] == "run"
    full = result["scope"] == "full" or release_required
    outputs = {"plan": json.dumps(result, sort_keys=True, separators=(",", ":")),
               "full": str(full).lower(), "windows": str(native and full).lower(),
               "deps": str(full or any(p == "Cargo.lock" or p.endswith("Cargo.toml") for p in paths)).lower()}
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
        for key, value in outputs.items():
            print(f"{key}={value}", file=stream)
    # A blocked plan is uploaded intact and rejected by the always aggregate.
    return result


def repository_commands():
    commands = [["cargo", "fmt", "--all", "--", "--check"]]
    for args in (["assets"], ["icons"], ["licences", "--check"], ["layers"], ["ui-containers"], ["lists"],
                 ["i18n", "--check"], ["mesh-decisions"], ["skills"], ["web-offline-test"]):
        commands.append(["cargo", "run", "--locked", "--package", "xtask", "--", *args])
    commands += [["cargo", "test", "-p", "xtask", name, "--locked"] for name in ("ui_containers", "mesh_decisions", "skills")]
    commands += [["bash", "scripts/test-format-pending.sh"]]
    commands += [["python3", "-I", "scripts/ci/" + name + ".py"] for name in
                 ("test_affected", "test_gate", "test_main_merge", "test_kernel_rig_plan")]
    for name in ("apprun", "flatpak", "nightly", "windows_icon"):
        path = Path("packaging/tests", name + "_test.py")
        gate.require(path.is_file() and path.stat().st_size > 0, "missing/empty packaging owning test: " + str(path))
        commands.append(["python3", "-I", "-c",
                         "import pathlib,sys,unittest; p=pathlib.Path(sys.argv[1]); "
                         "s=unittest.defaultTestLoader.discover(str(p.parent),pattern=p.name); "
                         "s.countTestCases() or sys.exit('empty packaging owning test selection: '+str(p)); "
                         "sys.exit(not unittest.TextTestRunner(verbosity=2).run(s).wasSuccessful())", str(path)])
    bot_tests = sorted(str(path) for area in ("claim", "triage", "kernel-paused", "pr-checks")
                       for path in Path("scripts", area).glob("*.test.js"))
    gate.require(bot_tests, "bot test selection is empty")
    return commands + [["node", "--test", *bot_tests]]


def wasm_commands():
    # Read the same checked-in layering table as xtask::wasm_check; do not keep a
    # second manually maintained crate list. Compile exactly the layered crates + web.
    table = Path("xtask/src/layers.rs").read_text(encoding="utf-8")
    names = re.findall(r'\("([\w-]+)", Class::Layer\(\d+\)\)', table)
    gate.require(names and len(names) == len(set(names)), "wasm layering table is unavailable/ambiguous")
    available = {p["name"] for p in affected.load_metadata(None)["packages"]}
    packages = sorted({"solidcraft-" + n for n in names} & available) + ["solidcraft-web"]
    gate.require(len(packages) > 1, "wasm package selection is empty")
    return [["cargo", "check", "--locked", "--target", "wasm32-unknown-unknown", "--profile", "ci",
             *[a for package in packages for a in ("-p", package)]]]


def execute_workflow(plan_file, job, out, *, full=False):
    raw = Path(plan_file).read_bytes()
    plan = gate.BoundPlan(raw)
    check_revision(plan)
    gate.require(plan["jobs"][job]["decision"] == "run", job + " is not selected")
    out = Path(out)
    out.mkdir(parents=True, exist_ok=True)
    if job == "repo":
        argvs = repository_commands()
    elif job == "pr-policy":
        # Event fields go through JSON, never executable shell interpolation.
        argvs = [["node", "-e", "const fs=require('node:fs'); const e=JSON.parse(fs.readFileSync(process.env.GITHUB_EVENT_PATH)); "
                  "const p=e.pull_request||{}; const g=e.merge_group||{}; "
                  "const r=require('./scripts/pr-checks/pr-checks.js').branchPolicy({"
                  "baseRef:p.base?.ref||g.base_ref,headRef:p.head?.ref||g.head_ref,"
                  "headRepo:p.head?.repo?.full_name,baseRepo:process.env.GITHUB_REPOSITORY,eventName:process.env.GITHUB_EVENT_NAME});"
                  "console.log(JSON.stringify(r)); process.exit(r.ok?0:1);"]]
    elif job == "rust":
        argvs = native_commands(plan, full=full or plan["scope"] == "full" or plan["releaseRequired"])
    elif job == "wasm":
        argvs = wasm_commands()
    else:
        raise ValueError(job + ": executable owner handoff is not integrated; no placeholder receipt is permitted")
    selection = junit = None
    if job == "rust":
        junit = Path("target/nextest/ci/junit.xml")
        junit.unlink(missing_ok=True)
        selection = out / "selection.json"
    commands = capture_commands(argvs, out)
    if junit and junit.exists():
        (out / "junit.xml").write_bytes(junit.read_bytes())
    receipt = gate.record_receipt(raw, job, commands, selection, junit,
                                  gate.read_known_failures(gate.KNOWN_FAILING) if job == "rust" else set())
    check_revision(plan)
    (out / "receipt.json").write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def aggregate_workflow(plan_file, needs_file, receipts_dir):
    plan = gate.BoundPlan(Path(plan_file).read_bytes())
    check_revision(plan)
    needs = gate.parse_json(Path(needs_file).read_bytes())
    results = required_results(needs)
    receipts = {}
    for path in Path(receipts_dir).rglob("receipt.json"):
        receipt = gate.parse_json(path.read_bytes())
        job = receipt.get("jobId")
        gate.require(job not in receipts, "duplicate receipt: " + str(job))
        receipts[job] = receipt
    problems = gate.evaluate(plan, results, receipts)
    if results.get("changes") != "success":
        problems.append("selection job did not succeed")
    full = plan["scope"] == "full" or plan["releaseRequired"]
    extras = ["clippy"] if plan["jobs"]["rust"]["decision"] == "run" else []
    if full and extras:
        extras += ["doc-tests", "oracle", "windows-fxc", "windows-startup", "property-tests",
                   "build-tests", "action-matrix", "action-matrix-determinism"]
    if full or needs.get("changes", {}).get("outputs", {}).get("deps") == "true":
        extras.append("dependency-advisories")
    if os.environ.get("REQUIRE_RISCV") == "true":
        extras.append("riscv64-gates")
    for job in extras:
        if results.get(job) != "success":
            problems.append(job + ": applicable job did not succeed: " + str(results.get(job, "missing")))
    gate.print_summary(plan, receipts, problems)
    return 1 if problems else 0


def workflow_main():
    parser = argparse.ArgumentParser(description="Source-bound fork workflow adapter (historical filename).")
    sub = parser.add_subparsers(dest="command", required=True)
    select = sub.add_parser("select")
    select.add_argument("--mode", default="affected")
    select.add_argument("--release-required", choices=("true", "false"), default="false")
    select.add_argument("--out", required=True)
    execute = sub.add_parser("execute")
    execute.add_argument("--job", required=True)
    execute.add_argument("--out", required=True)
    execute.add_argument("--full", action="store_true")
    aggregate = sub.add_parser("aggregate")
    aggregate.add_argument("--needs", required=True)
    aggregate.add_argument("--receipts", required=True)
    for p in (execute, aggregate):
        p.add_argument("--plan", required=True)
    args = parser.parse_args()
    try:
        if args.command == "select":
            select_workflow(json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_bytes()), args.mode,
                            args.release_required == "true", args.out)
        elif args.command == "execute":
            execute_workflow(args.plan, args.job, args.out, full=args.full)
        else:
            return aggregate_workflow(args.plan, args.needs, args.receipts)
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        print("fork CI: " + str(error), file=sys.stderr)
        return 1
    return 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", required=True)
    ap.add_argument("--head", required=True)
    ap.add_argument("--repo", required=True)
    ap.add_argument("--pr", required=True, type=int)
    ap.add_argument("--head-repo", required=True)
    ap.add_argument("--head-ref", required=True)
    ap.parse_args()
    print("carried=")


if __name__ == "__main__":
    if sys.argv[1:2] and sys.argv[1] in ("select", "execute", "aggregate"):
        sys.exit(workflow_main())
    main()
