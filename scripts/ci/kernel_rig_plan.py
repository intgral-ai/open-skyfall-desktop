#!/usr/bin/env python3
"""Select the kernel rig areas a pull request affects (.github/workflows/kernel-rig.yml).

Usage: kernel_rig_plan.py (--all | --paths FILE [--pin-diff DIFF]) [--root DIR] [--github-output FILE]
       Add --expected-revision SHA --out FILE for an immutable execution plan.
       build/shard/verify/verify-handoff execute or check the existing Rust rig.

FILE holds the changed paths relative to the repository root, separated by NUL (git diff -z);
"-" reads standard input. DIFF is the pull request's unified diff of the root Cargo.toml and
Cargo.lock (PIN_FILES). --all selects every area (pushes, merge groups, the release gate, a run
by hand). --root is the repository (default: two folders up from this script).

The areas are the folders of tests/kernel-cases/ (the ladder aside), so a new area joins by
having a folder with its cases, its KNOWN_FAILING and its PATHS. A changed path selects:
- every area, when it matches tests/kernel-cases/PATHS-ALL (the shared core: SMLib, geom, the
  kernel's core, the rig's grader and runner, the workflow);
- every area, when the solidcore pin moves: a changed line of DIFF names solidcore's repository
  (the root Cargo.toml's git rev, Cargo.lock's source). Another dependency's change in those
  files runs none, so a pin is told apart by its lines, which a path filter cannot see;
- an area, when it is under tests/kernel-cases/<area>/ or matches that area's PATHS: one git
  pathspec glob a line (`*` within a folder, `**` across folders), `#` starts a comment.
The ladder spans every operation: it runs whenever any area does, and on a change under
tests/kernel-cases/ladder/.

The coverage check (`cargo xtask kernel-cases --coverage`) runs in the build job. The build runs
when a shard does, or when the command registry or the cases may have changed (COVERAGE below),
so a new command without a case fails on its own pull request.

Prints and optionally appends to $GITHUB_OUTPUT:
  shards=["booleans", ..., "ladder"]   the shard matrix, JSON (empty list: no shard runs)
  build=true|false                     the build job (and the coverage check) runs

Selection alone is applicability, not execution evidence. The workflow adapter
keeps real command exits, binary hashes and original results.json case paths;
only the existing Rust coverage/known-failing/ladder graders decide acceptance.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from gate import parse_json, require  # noqa: E402

CASES = "tests/kernel-cases"
LADDER = "ladder"
# Changes here may add or remove a registry command, or the case that covers one: the coverage
# check must see them even when no area runs.
COVERAGE = (
    "crates/engine/src/cmd/**",
    "tests/kernel-cases/EXEMPT",
    "tests/kernel-cases/**/recipe.json",
    "xtask/src/kernel_cases.rs",
)

# The files that pin solidcore, and the repository a pin's lines name.
PIN_FILES = ("Cargo.toml", "Cargo.lock")
SOLIDCORE = "github.com/tangency-labs/solidcore"


def pin_moved(diff):
    """Whether a unified diff of PIN_FILES changes a line that names solidcore's repository (an
    added or removed line, not its file headers or context)."""
    return any(
        line.startswith(("+", "-")) and not line.startswith(("+++ ", "--- ")) and SOLIDCORE in line for line in diff.splitlines()
    )


def glob_regex(glob):
    """A git pathspec glob (`:(glob)` magic) as a regular expression over a whole path: `**/`
    matches any folders (none too), a trailing `/**` everything below, `*` and `?` stay within
    one folder."""
    out, i = [], 0
    while i < len(glob):
        if glob.startswith("**/", i):
            out.append("(?:.*/)?")
            i += 3
        elif glob.startswith("**", i):
            out.append(".*")
            i += 2
        elif glob[i] == "*":
            out.append("[^/]*")
            i += 1
        elif glob[i] == "?":
            out.append("[^/]")
            i += 1
        else:
            out.append(re.escape(glob[i]))
            i += 1
    return re.compile("".join(out) + r"\Z")


def read_globs(path):
    """The globs of a PATHS file: one a line, blank lines and `#` comments skipped."""
    with open(path, encoding="utf-8") as f:
        lines = [line.strip() for line in f]
    globs = [line for line in lines if line and not line.startswith("#")]
    for g in globs:
        if g.startswith("/") or g.startswith(":") or "\\" in g:
            raise SystemExit(f"{path}: `{g}`: a glob is relative to the repository root, with / and no pathspec magic")
    return globs


def areas(root):
    """Every area (a folder of tests/kernel-cases/ but the ladder) with its PATHS globs."""
    base = os.path.join(root, CASES)
    out = {}
    for name in sorted(os.listdir(base)):
        if name == LADDER or not os.path.isdir(os.path.join(base, name)):
            continue
        paths = os.path.join(base, name, "PATHS")
        if not os.path.isfile(paths):
            raise SystemExit(f"{CASES}/{name}/PATHS is missing: every area lists the source files whose change reruns it")
        out[name] = read_globs(paths)
    return out


def matches(globs, path):
    return any(glob_regex(g).match(path) for g in globs)


def select(root, paths, everything=False, pin_diff=""):
    """Return (shards, build): the areas to run, in order and the ladder last, and whether the
    build job runs. `pin_diff` is the diff of PIN_FILES."""
    by_area = areas(root)
    if everything or pin_moved(pin_diff):
        return [*by_area, LADDER], True
    shared = read_globs(os.path.join(root, CASES, "PATHS-ALL"))
    hit, ladder = set(), False
    for path in paths:
        if path == "scripts/ci/gate.py" or matches(shared, path):
            return [*by_area, LADDER], True
        if path.startswith(f"{CASES}/{LADDER}/"):
            ladder = True
        for area, globs in by_area.items():
            if path.startswith(f"{CASES}/{area}/") or matches(globs, path):
                hit.add(area)
    shards = [a for a in by_area if a in hit]
    if shards or ladder:
        shards.append(LADDER)
    return shards, bool(shards) or any(matches(COVERAGE, p) for p in paths)


def read_paths(src):
    data = sys.stdin.buffer.read() if src == "-" else open(src, "rb").read()
    return [p.decode() for p in data.split(b"\0") if p]


# This is a thin execution/evidence adapter around select() and the existing Rust
# coverage/known-failing/ladder graders, not another geometry or baseline grader.
BINARIES = ("target/rig/release/solidcraft-cli", "target/rig/debug/xtask")
BUILD_COMMANDS = [["cargo", "build", "--release", "--locked", "-p", "solidcraft-cli"],
                  ["cargo", "build", "--locked", "-p", "xtask"],
                  [BINARIES[1], "kernel-cases", "--coverage", "--cli", BINARIES[0]]]


def digest(path):
    result = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(block)
    return result.hexdigest()


def checkout(root, revision):
    require(isinstance(revision, str) and re.fullmatch("[0-9a-f]{40}", revision), "invalid sourceRevision")
    actual = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    require(actual == revision, "rig sourceRevision mismatch")
    require(subprocess.run(["git", "diff", "--quiet", "HEAD", "--"], cwd=root).returncode == 0, "dirty tracked rig source")


def expected_cases(root, shards):
    base, result = Path(root) / CASES, {}
    for shard in shards:
        if shard == LADDER:
            manifest = parse_json((base / LADDER / "ladder.json").read_bytes())
            paths = [path for tier in manifest["tiers"] for path in tier["cases"]]
        else:
            paths = [p.relative_to(base).as_posix() for p in sorted((base / shard).iterdir())
                     if p.is_dir() and (p / "recipe.json").is_file() and (p / "expected.json").is_file()]
        require(paths and all(isinstance(p, str) and p and not p.startswith("/") and ".." not in p.split("/")
                              and "\\" not in p and (base / p / "recipe.json").is_file()
                              and (base / p / "expected.json").is_file() for p in paths), shard + ": empty/invalid case selection")
        require(len(paths) == len(set(paths)), shard + ": duplicate expected case")
        result[shard] = sorted(paths)
    return result


def load_execution_plan(root, path, revision, *, full=False):
    checkout(root, revision)
    value = parse_json(Path(path).read_bytes())
    require(value.get("version") == 1 and value.get("sourceRevision") == revision, "plan sourceRevision/version mismatch")
    require(type(value.get("full")) is bool and type(value.get("build")) is bool, "invalid rig plan decisions")
    all_shards, _ = select(root, [], everything=True)
    shards = value.get("shards")
    require(isinstance(shards, list) and shards == [s for s in all_shards if s in shards], "invalid/duplicate rig shards")
    require(not shards or value["build"], "selected shards require a build")
    require(not (full or value["full"]) or (value["full"] and value["build"] and shards == all_shards),
            "full rig requires every area and ladder")
    require(value.get("expectedCases") == expected_cases(root, shards), "rig expected case selection mismatch")
    return value


def write_json(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def run_commands(root, argv_list):
    commands = []
    for argv in argv_list:
        # Fixed target dir isolates the workflow's tools and is the exact path
        # packaged below. Do not invoke a cached xtask from another worktree.
        status = subprocess.run(argv, cwd=root, env={**os.environ, "CARGO_TARGET_DIR": str(Path(root) / "target/rig")}).returncode
        commands.append(dict(argv=argv, exitCode=status))
        if status != 0:
            break
    return commands


def validate_commands(commands, argv_list):
    require(isinstance(commands, list) and [c.get("argv") for c in commands] == argv_list, "rig command evidence mismatch")
    require(all(type(c.get("exitCode")) is int and c["exitCode"] == 0 for c in commands), "rig command/grader failed")


def validate_build(root, plan_path, revision, file, *, binaries):
    value = parse_json(Path(file).read_bytes())
    require(value.get("sourceRevision") == revision, "build sourceRevision mismatch")
    require(value.get("planDigest") == digest(plan_path), "build planDigest mismatch")
    require(value.get("checkoutRoot") == str(Path(root).resolve()), "binary checkout root mismatch")
    validate_commands(value.get("commands"), BUILD_COMMANDS)
    hashes = value.get("binaries")
    require(isinstance(hashes, dict) and set(hashes) == set(BINARIES)
            and all(isinstance(h, str) and re.fullmatch("[0-9a-f]{64}", h) for h in hashes.values()), "missing binary digests")
    if binaries:
        for path, expected in hashes.items():
            require(digest(Path(root) / path) == expected, "binary digest mismatch: " + path)
    return value


def build_workflow(root, plan_path, revision, out):
    value = load_execution_plan(root, plan_path, revision)
    require(value["build"], "rig build not selected")
    commands = run_commands(root, BUILD_COMMANDS)
    checkout(root, revision)
    write_json(Path(out) / "build.json", dict(sourceRevision=revision, planDigest=digest(plan_path),
        checkoutRoot=str(Path(root).resolve()), commands=commands,
        binaries={p: digest(Path(root) / p) for p in BINARIES if (Path(root) / p).is_file()}))
    validate_build(root, plan_path, revision, Path(out) / "build.json", binaries=True)


def shard_command(shard, out):
    grader = ["--ladder", "--gate", "--jobs", "1"] if shard == LADDER else [
        "--area", shard, "--known-failing", f"{CASES}/{shard}/KNOWN_FAILING", "--jobs", "2"]
    return [BINARIES[1], "kernel-cases", *grader, "--cli", BINARIES[0], "--out", str(out)]


def validate_results(plan, shard, path):
    rows = parse_json(Path(path).read_bytes())
    require(isinstance(rows, list) and rows, shard + ": empty results")
    require(all(isinstance(r, dict) and isinstance(r.get("path"), str)
                and r.get("status") in ("pass", "fail", "crash", "hang", "unsupported") for r in rows),
            shard + ": skipped/invalid case outcome")
    paths = [r["path"] for r in rows]
    require(len(paths) == len(set(paths)), shard + ": duplicate case outcome")
    require(sorted(paths) == plan["expectedCases"][shard], shard + ": missing/unexpected case outcomes")
    return rows


def shard_workflow(root, plan_path, revision, shard, out):
    value = load_execution_plan(root, plan_path, revision)
    require(shard in value["shards"], "rig shard not selected")
    build = Path(root) / "rig-build/build.json"
    validate_build(root, plan_path, revision, build, binaries=True)
    directory = Path(root) / out
    require(not (directory / "results.json").exists(), "refuse stale rig results")
    command = shard_command(shard, out)
    commands = run_commands(root, [command])
    checkout(root, revision)
    results = directory / "results.json"
    if results.is_file():
        write_json(directory / "receipt.json", dict(sourceRevision=revision, planDigest=digest(plan_path),
            buildDigest=digest(build), shard=shard, commands=commands, resultsDigest=digest(results), outputDirectory=str(out)))
    validate_commands(commands, [command])
    validate_results(value, shard, results)


def verify_evidence(root, plan_path, revision, needs_path, evidence, *, full=False, binaries=True):
    value = load_execution_plan(root, plan_path, revision, full=full)
    needs = parse_json(Path(needs_path).read_bytes())
    for job, expected in (("plan", "success"), ("build", "success" if value["build"] else "skipped"),
                          ("shard", "success" if value["shards"] else "skipped")):
        require(needs.get(job, {}).get("result") == expected, job + ": real job result is not " + expected)
    evidence = Path(evidence)
    build = evidence / "rig-build/build.json"
    if value["build"]:
        validate_build(root, plan_path, revision, build, binaries=binaries)
    count, raw_failures = 0, []
    found = {p.parent.name for p in (evidence / "rig-results").glob("*/receipt.json")}
    require(found == set(value["shards"]), "missing/unexpected rig shard receipts")
    for shard in value["shards"]:
        directory = evidence / "rig-results" / shard
        receipt = parse_json((directory / "receipt.json").read_bytes())
        require(receipt.get("sourceRevision") == revision and receipt.get("shard") == shard, "shard sourceRevision/identity mismatch")
        require(receipt.get("planDigest") == digest(plan_path) and receipt.get("buildDigest") == digest(build), "shard plan/build digest mismatch")
        # Output directory is a fixed workflow path, not a producer-selected command.
        out = "rig-results/" + shard
        require(receipt.get("outputDirectory") == out, "shard output directory mismatch")
        validate_commands(receipt.get("commands"), [shard_command(shard, out)])
        results = directory / "results.json"
        require(receipt.get("resultsDigest") == digest(results), "raw results digest mismatch")
        rows = validate_results(value, shard, results)
        count += len(rows)
        raw_failures.extend(shard + ":" + r["path"] + ":" + r["status"] for r in rows if r["status"] != "pass")
    if value["shards"]:
        print(f"kernel-rig: {count} case outcomes; selected existing coverage/area/ladder graders satisfied; raw nonpass: {raw_failures}")
    elif value["build"]:
        print("kernel-rig: coverage command satisfied; no area/ladder selected (not full release evidence)")
    else:
        print("kernel-rig: not_applicable (no affected area or command coverage input; no build/tests executed)")
    return value


def verify_handoff(root, directory, revision):
    # The nested aggregate already checked the physical binary digests; the
    # release caller also requires its actual workflow success. Recheck its raw
    # source-bound reports, full plan, command receipts and actual nested needs,
    # not a producer's constant pass/count/status flag. No second grader exists.
    directory = Path(directory)
    return verify_evidence(root, directory / "plan.json", revision, directory / "needs.json", directory,
                           full=True, binaries=False)


def workflow_main():
    parser = argparse.ArgumentParser(description="Existing kernel rig execution/evidence adapter")
    parser.add_argument("mode", choices=("build", "shard", "verify", "verify-handoff"))
    parser.add_argument("--root", default=str(Path(__file__).resolve().parents[2]))
    parser.add_argument("--expected-revision", required=True)
    for flag in ("plan", "out", "shard", "needs", "evidence", "handoff"):
        parser.add_argument("--" + flag)
    parser.add_argument("--require-full", action="store_true")
    args = parser.parse_args()
    root = Path(args.root).resolve()
    try:
        if args.mode == "build":
            build_workflow(root, args.plan, args.expected_revision, args.out)
        elif args.mode == "shard":
            shard_workflow(root, args.plan, args.expected_revision, args.shard, args.out)
        elif args.mode == "verify-handoff":
            verify_handoff(root, args.handoff, args.expected_revision)
        else:
            value = verify_evidence(root, args.plan, args.expected_revision, args.needs, args.evidence, full=args.require_full)
            if args.handoff:
                directory = Path(args.handoff)
                directory.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(args.plan, directory / "plan.json")
                shutil.copyfile(args.needs, directory / "needs.json")
                if value["build"]:
                    target = directory / "rig-build/build.json"
                    target.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(Path(args.evidence) / "rig-build/build.json", target)
                for shard in value["shards"]:
                    target = directory / "rig-results" / shard
                    target.mkdir(parents=True, exist_ok=True)
                    for name in ("receipt.json", "results.json"):
                        shutil.copyfile(Path(args.evidence) / "rig-results" / shard / name, target / name)
    except (OSError, ValueError, TypeError, KeyError, subprocess.CalledProcessError) as error:
        print("kernel-rig evidence: " + str(error), file=sys.stderr)
        return 1
    return 0


def main():
    ap = argparse.ArgumentParser()
    group = ap.add_mutually_exclusive_group(required=True)
    group.add_argument("--paths")
    group.add_argument("--all", action="store_true")
    ap.add_argument("--pin-diff")
    ap.add_argument("--root", default=os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))
    ap.add_argument("--github-output")
    ap.add_argument("--expected-revision")
    ap.add_argument("--out")
    a = ap.parse_args()
    pin_diff = ""
    if a.pin_diff:
        with open(a.pin_diff, encoding="utf-8", errors="replace") as f:
            pin_diff = f.read()
    shards, build = select(a.root, [] if a.all else read_paths(a.paths), everything=a.all, pin_diff=pin_diff)
    if a.out:
        checkout(a.root, a.expected_revision)
        write_json(a.out, dict(version=1, sourceRevision=a.expected_revision, full=a.all, shards=shards,
                              build=build, expectedCases=expected_cases(a.root, shards)))
    lines = [f"shards={json.dumps(shards)}", f"build={'true' if build else 'false'}"]
    print("\n".join(lines))
    if a.github_output:
        with open(a.github_output, "a", encoding="utf-8") as f:
            f.write("\n".join(lines) + "\n")


if __name__ == "__main__":
    if sys.argv[1:2] and sys.argv[1] in ("build", "shard", "verify", "verify-handoff"):
        sys.exit(workflow_main())
    main()
