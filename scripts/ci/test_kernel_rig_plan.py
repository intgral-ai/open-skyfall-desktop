#!/usr/bin/env python3
"""Self-test for kernel_rig_plan.py. Run: python3 -I scripts/ci/test_kernel_rig_plan.py (from the
repo root).

Uses a small synthetic tests/kernel-cases/, then the real one: every area has its PATHS and
KNOWN_FAILING, every glob names files that exist, and the workflow's trigger paths cover them all.
"""
import os
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import kernel_rig_plan as plan  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
WORKFLOW = os.path.join(ROOT, ".github", "workflows", "kernel-rig.yml")


def write(root, rel, text=""):
    path = os.path.join(root, rel)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)


class Glob(unittest.TestCase):
    def test_star_stays_in_a_folder_and_double_star_crosses(self):
        self.assertTrue(plan.glob_regex("crates/kernel/src/blend*.rs").match("crates/kernel/src/blend_smlib.rs"))
        self.assertFalse(plan.glob_regex("crates/kernel/src/*.rs").match("crates/kernel/src/tess/grid.rs"))
        self.assertTrue(plan.glob_regex("crates/kernel/src/tess/**").match("crates/kernel/src/tess/a/b.rs"))
        self.assertFalse(plan.glob_regex("crates/kernel/src/tess/**").match("crates/kernel/src/tess.rs"))
        self.assertTrue(plan.glob_regex("tests/**/recipe.json").match("tests/recipe.json"))
        self.assertTrue(plan.glob_regex("tests/**/recipe.json").match("tests/a/b/recipe.json"))
        self.assertFalse(plan.glob_regex("a.rs").match("a.rsx"))
        self.assertFalse(plan.glob_regex("a.rs").match("b/a.rs"))


class Synthetic(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        r = self.root = self.tmp.name
        write(r, "tests/kernel-cases/PATHS-ALL", "# shared\ncrates/geom/**\n\n.github/workflows/kernel-rig.yml\n")
        write(r, "tests/kernel-cases/fillets/PATHS", "# blends\ncrates/kernel/src/blend*.rs\n")
        write(r, "tests/kernel-cases/shell/PATHS", "crates/kernel/src/offset*.rs\ncrates/kernel/src/blend_smlib.rs\n")
        write(r, "tests/kernel-cases/ladder/ladder.json", "{}")
        write(r, "tests/kernel-cases/README.md")

    def tearDown(self):
        self.tmp.cleanup()

    def select(self, *paths):
        return plan.select(self.root, list(paths))

    def test_an_area_runs_on_its_paths_with_the_ladder(self):
        self.assertEqual(self.select("crates/kernel/src/blend.rs"), (["fillets", "ladder"], True))
        self.assertEqual(self.select("crates/kernel/src/blend_smlib.rs"), (["fillets", "shell", "ladder"], True))

    def test_a_change_to_an_areas_cases_runs_it(self):
        self.assertEqual(self.select("tests/kernel-cases/shell/1-x/expected.json"), (["shell", "ladder"], True))
        self.assertEqual(self.select("tests/kernel-cases/shell/PATHS"), (["shell", "ladder"], True))
        self.assertEqual(self.select("tests/kernel-cases/ladder/golden.json"), (["ladder"], True))

    def test_the_shared_core_runs_every_area(self):
        for p in ["crates/geom/src/brep.rs", ".github/workflows/kernel-rig.yml", "scripts/ci/gate.py"]:
            self.assertEqual(self.select("docs/a.md", p), (["fillets", "shell", "ladder"], True), p)

    def test_unrelated_changes_run_nothing(self):
        self.assertEqual(self.select("crates/ui-egui/src/app.rs", "docs/a.md", "tests/kernel-cases/README.md"), ([], False))

    def test_a_registry_or_case_change_builds_for_the_coverage_check(self):
        self.assertEqual(self.select("crates/engine/src/cmd/view_navigation.rs"), ([], True))
        self.assertEqual(self.select("tests/kernel-cases/EXEMPT"), ([], True))

    def test_a_solidcore_pin_change_runs_every_area(self):
        moved = (
            "--- a/Cargo.lock\n+++ b/Cargo.lock\n@@ -1,3 +1,3 @@\n name = \"solidcore\"\n"
            '-source = "git+https://github.com/tangency-labs/solidcore?rev=97ed188#97ed188"\n'
            '+source = "git+https://github.com/tangency-labs/solidcore?rev=8ddce90#8ddce90"\n'
        )
        everything = (["fillets", "shell", "ladder"], True)
        self.assertEqual(plan.select(self.root, ["Cargo.lock", "Cargo.toml"], pin_diff=moved), everything)

    def test_another_dependency_change_runs_nothing(self):
        bumped = '--- a/Cargo.lock\n+++ b/Cargo.lock\n@@ -1,2 +1,2 @@\n name = "serde"\n-version = "1.0.1"\n+version = "1.0.2"\n'
        self.assertEqual(plan.select(self.root, ["Cargo.lock", "Cargo.toml"], pin_diff=bumped), ([], False))
        # A context line that names solidcore is no change to its pin.
        context = '--- a/Cargo.toml\n+++ b/Cargo.toml\n@@ -1,2 +1,2 @@\n solidcore = { git = "https://github.com/tangency-labs/solidcore" }\n-a = 1\n+a = 2\n'
        self.assertEqual(plan.select(self.root, ["Cargo.toml"], pin_diff=context), ([], False))

    def test_everything_runs_every_area(self):
        self.assertEqual(plan.select(self.root, [], everything=True), (["fillets", "shell", "ladder"], True))

    def test_a_new_area_joins_from_its_folder(self):
        write(self.root, "tests/kernel-cases/surfaces/PATHS", "crates/kernel/src/surface_*.rs\n")
        self.assertEqual(self.select("crates/kernel/src/surface_trim.rs"), (["surfaces", "ladder"], True))
        self.assertEqual(plan.select(self.root, [], everything=True)[0], ["fillets", "shell", "surfaces", "ladder"])

    def test_an_area_without_paths_is_an_error(self):
        os.makedirs(os.path.join(self.root, "tests/kernel-cases/plastic"))
        with self.assertRaises(SystemExit) as e:
            self.select("crates/geom/src/brep.rs")
        self.assertIn("plastic/PATHS is missing", str(e.exception))


def trigger_globs():
    """The pull_request trigger paths of the workflow (the push trigger lists the same)."""
    with open(WORKFLOW, encoding="utf-8") as f:
        text = f.read()
    events = {}
    for event in ("pull_request", "push"):
        block = re.search(rf"^  {event}:\n((?:    .*\n|\s*\n)+)", text, re.M)
        assert block, event
        paths = block.group(1).split("paths:", 1)[1]
        events[event] = re.findall(r'^\s+- "([^"]+)"', paths, re.M)
    return events


def sample(glob):
    """A path the glob matches: each wildcard filled in."""
    return glob.replace("**", "x/y").replace("*", "x").replace("?", "x")


class Real(unittest.TestCase):
    def test_every_area_has_its_lists_and_every_glob_names_files(self):
        # Tracked files and new ones not yet committed (not the ignored ones).
        ls = ["git", "ls-files", "--cached", "--others", "--exclude-standard"]
        tracked = subprocess.run(ls, cwd=ROOT, check=True, capture_output=True, text=True).stdout.split("\n")
        shared = plan.read_globs(os.path.join(ROOT, plan.CASES, "PATHS-ALL"))
        areas = plan.areas(ROOT)
        self.assertTrue(areas)
        for area, globs in {"PATHS-ALL": shared, **areas}.items():
            if area != "PATHS-ALL":
                self.assertTrue(os.path.isfile(os.path.join(ROOT, plan.CASES, area, "KNOWN_FAILING")), area)
            for g in globs:
                with self.subTest(area=area, glob=g):
                    rx = plan.glob_regex(g)
                    self.assertTrue(any(rx.match(t) for t in tracked), f"{area}: {g} matches no file")

    def test_an_area_names_only_its_own_files(self):
        # The shared core is in PATHS-ALL; an area's PATHS repeating it (or a whole crate's
        # folder) would only blur what the area is about.
        ls = ["git", "ls-files", "--cached", "--others", "--exclude-standard"]
        files = subprocess.run(ls, cwd=ROOT, check=True, capture_output=True, text=True).stdout.split("\n")
        shared = plan.read_globs(os.path.join(ROOT, plan.CASES, "PATHS-ALL"))
        core = {f for f in files if plan.matches(shared, f)}
        for area, globs in plan.areas(ROOT).items():
            for g in globs:
                rx = plan.glob_regex(g)
                overlap = sorted(f for f in core if rx.match(f))
                with self.subTest(area=area, glob=g):
                    self.assertFalse(overlap, f"{area}/PATHS: {g} names shared-core files (PATHS-ALL): {overlap[:3]}")

    def test_the_workflow_triggers_on_every_glob(self):
        events = trigger_globs()
        self.assertEqual(events["pull_request"], events["push"])
        triggers = [plan.glob_regex(g) for g in events["pull_request"]]
        globs = [*plan.read_globs(os.path.join(ROOT, plan.CASES, "PATHS-ALL")), *plan.COVERAGE]
        for area, own in plan.areas(ROOT).items():
            globs += [*own, f"{plan.CASES}/{area}/**"]
        for g in globs:
            with self.subTest(glob=g):
                self.assertTrue(any(t.match(sample(g)) for t in triggers), f"{g}: not under the workflow's trigger paths")

    def test_the_workflow_triggers_on_the_pin_files(self):
        triggers = [plan.glob_regex(g) for g in trigger_globs()["pull_request"]]
        for f in plan.PIN_FILES:
            with self.subTest(file=f):
                self.assertTrue(any(t.match(f) for t in triggers), f"{f}: not under the workflow's trigger paths")

    def test_a_ui_change_runs_nothing(self):
        self.assertEqual(plan.select(ROOT, ["crates/ui-egui/src/app.rs", "docs/kernel-cases.md", "README.md"]), ([], False))


class WorkflowEvidence(unittest.TestCase):
    """Real adapter/subprocess boundaries; fixture binaries are not CAD evidence."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix=".rig-adapter-test-", dir=ROOT)
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        for name in ("fillets", "shell"):
            write(self.root, f"tests/kernel-cases/{name}/PATHS", f"crates/kernel/src/{name}.rs\n")
            write(self.root, f"tests/kernel-cases/{name}/KNOWN_FAILING")
            write(self.root, f"tests/kernel-cases/{name}/one/recipe.json", "{}")
            write(self.root, f"tests/kernel-cases/{name}/one/expected.json", "{}")
        write(self.root, "tests/kernel-cases/PATHS-ALL", ".github/workflows/kernel-rig.yml\n")
        write(self.root, "tests/kernel-cases/ladder/tier/two/recipe.json", "{}")
        write(self.root, "tests/kernel-cases/ladder/tier/two/expected.json", "{}")
        write(self.root, "tests/kernel-cases/ladder/ladder.json", json.dumps({"tiers": [
            {"tier": "tier", "what": "fixture", "cases": ["fillets/one", "ladder/tier/two"]}]}))
        subprocess.run(["git", "init", "-q", str(self.root)], check=True)
        subprocess.run(["git", "-C", str(self.root), "add", "tests"], check=True)
        subprocess.run(["git", "-C", str(self.root), "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                        "commit", "-q", "-m", "rig source"], check=True)
        self.sha = subprocess.check_output(["git", "-C", str(self.root), "rev-parse", "HEAD"], text=True).strip()
        (self.root / "scripts").mkdir()
        (self.root / "scripts/ci").symlink_to(Path(ROOT) / "scripts/ci", target_is_directory=True)
        self.env = dict(os.environ, GITHUB_SHA=self.sha, EVENT_NAME="pull_request", FULL_VALIDATION="true",
                        BASE_SHA=self.sha, HEAD_SHA=self.sha, RUNNER_TEMP=str(self.root),
                        GITHUB_OUTPUT=str(self.root / "outputs"))
        self.tools = self.root / "fixture-tools"
        self.tools.mkdir()
        # Replace the expensive native compiler/runner boundary only. Actual
        # Python adapter, git, raw files, hashes and child exit codes stay real.
        runner = '''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
args=sys.argv[1:]
if args[:2] != ['kernel-cases', '--coverage'] and '--out' not in args:
    sys.exit('unexpected fixture runner invocation')
if '--coverage' in args:
    sys.exit(1 if os.environ.get('RIG_FIXTURE_MODE') == 'coverage-fail' else 0)
base=Path('tests/kernel-cases')
if '--ladder' in args:
    if '--gate' not in args: sys.exit('missing real ladder grader flag')
    paths=['fillets/one', 'ladder/tier/two']
else:
    area=args[args.index('--area')+1]
    if args[args.index('--known-failing')+1] != f'tests/kernel-cases/{area}/KNOWN_FAILING':
        sys.exit('wrong reviewed grader')
    paths=[f'{area}/one']
rows=[dict(path=p, case=p.split('/')[-1], area=p.split('/')[0], status='pass') for p in paths]
mode=os.environ.get('RIG_FIXTURE_MODE')
if mode == 'empty': rows=[]
if mode == 'partial': rows=rows[:-1]
if mode == 'duplicate': rows+=rows[:1]
if mode == 'skipped': rows[0]['status']='skipped'
if mode == 'failed': rows[0]['status']='fail'
out=Path(args[args.index('--out')+1]); out.mkdir(parents=True,exist_ok=True)
(out/'results.json').write_text(json.dumps(rows))
sys.exit(1 if mode == 'failed' else 0)
'''
        cargo = self.tools / "cargo"
        cargo.write_text("#!/usr/bin/env python3\nimport os,sys\nfrom pathlib import Path\n"
            "args=sys.argv[1:]\n"
            "if args not in (['build','--release','--locked','-p','solidcraft-cli'], ['build','--locked','-p','xtask']): sys.exit('unexpected cargo args')\n"
            "base=Path('target/rig'); (base/'release').mkdir(parents=True,exist_ok=True); (base/'debug').mkdir(parents=True,exist_ok=True)\n"
            "for path,body in ((base/'release/solidcraft-cli', '#!/bin/sh\\nexit 0\\n'), (base/'debug/xtask', " + repr(runner) + ")):\n"
            "    path.write_text(body); path.chmod(0o755)\n")
        cargo.chmod(0o755)
        self.env["PATH"] = str(self.tools) + os.pathsep + os.environ["PATH"]

    def cli(self, *args, mode=None):
        return subprocess.run([sys.executable, "-I", str(Path(ROOT) / "scripts/ci/kernel_rig_plan.py"), *map(str, args),
                               "--root", str(self.root)], cwd=self.root,
                              env={**self.env, **({"RIG_FIXTURE_MODE": mode} if mode else {})}, capture_output=True, text=True)

    def select_workflow(self, full=True):
        text = Path(WORKFLOW).read_text()
        block = text.split("      - id: plan\n", 1)[1].split("\n  build:", 1)[0]
        script = block.split("        run: |\n", 1)[1].split("\n      -", 1)[0]
        script = "\n".join(line[10:] for line in script.splitlines())
        result = subprocess.run(["bash", "-e", "-o", "pipefail", "-c", script], cwd=self.root,
            env={**self.env, "FULL_VALIDATION": "true" if full else "false"}, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return dict(line.split("=", 1) for line in (self.root / "outputs").read_text().splitlines())

    def prepared(self):
        self.select_workflow()
        path = self.root / "rig-plan/plan.json"
        self.assertTrue(path.is_file(), "selected rig needs a source-bound execution plan")
        build = self.cli("build", "--plan", path, "--expected-revision", self.sha, "--out", "rig-build")
        self.assertEqual(build.returncode, 0, build.stdout + build.stderr)
        return path

    def complete(self):
        path = self.prepared()
        for shard in ("fillets", "shell", "ladder"):
            result = self.cli("shard", "--plan", path, "--expected-revision", self.sha,
                              "--shard", shard, "--out", "rig-results/" + shard)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        (self.root / "needs.json").write_text(json.dumps({job: {"result": "success"} for job in ("plan", "build", "shard")}))
        return path

    def verify(self, path, *, handoff=None):
        return self.cli("verify", "--plan", path, "--expected-revision", self.sha, "--needs", "needs.json",
                        "--evidence", ".", "--require-full", *(["--handoff", handoff] if handoff else []))

    def test_full_release_call_overrides_empty_pr_diff_and_draft_skip(self):
        values = self.select_workflow()
        self.assertEqual(json.loads(values["shards"]), ["fillets", "shell", "ladder"])
        data = json.loads((self.root / "rig-plan/plan.json").read_text())
        self.assertEqual(data["sourceRevision"], self.sha)
        self.assertTrue(data["full"])
        text = Path(WORKFLOW).read_text()
        self.assertRegex(text, r"(?m)^    if: inputs.full_validation \|\| github.event_name != 'pull_request'")

    def test_ordinary_unaffected_pr_keeps_explicit_empty_selection(self):
        values = self.select_workflow(full=False)
        self.assertEqual(json.loads(values["shards"]), [])
        self.assertEqual(values["build"], "false")
        path = self.root / "rig-plan/plan.json"
        self.assertTrue(path.is_file(), "even an empty selection needs its source-bound plan")
        self.assertEqual(json.loads(path.read_text())["sourceRevision"], self.sha)
        (self.root / "needs.json").write_text(json.dumps({"plan": {"result": "success"},
            "build": {"result": "skipped"}, "shard": {"result": "skipped"}}))
        result = self.cli("verify", "--plan", path, "--expected-revision", self.sha,
                          "--needs", "needs.json", "--evidence", ".")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("not_applicable", result.stdout)
        self.assertNotIn("graders satisfied", result.stdout)

    def test_unavailable_pr_comparison_selects_full_instead_of_empty_success(self):
        self.env["BASE_SHA"] = "0" * 40
        values = self.select_workflow(full=False)
        self.assertEqual(json.loads(values["shards"]), ["fillets", "shell", "ladder"])
        self.assertTrue(json.loads((self.root / "rig-plan/plan.json").read_text())["full"])

    def test_ordinary_registry_change_runs_coverage_even_without_shards(self):
        write(self.root, "crates/engine/src/cmd/new.rs", "// fixture command\n")
        subprocess.run(["git", "-C", str(self.root), "add", "crates"], check=True)
        subprocess.run(["git", "-C", str(self.root), "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                        "commit", "-q", "-m", "registry fixture"], check=True)
        self.sha = subprocess.check_output(["git", "-C", str(self.root), "rev-parse", "HEAD"], text=True).strip()
        self.env.update(GITHUB_SHA=self.sha, HEAD_SHA=self.sha)
        values = self.select_workflow(full=False)
        self.assertEqual(json.loads(values["shards"]), [])
        self.assertEqual(values["build"], "true")
        path = self.root / "rig-plan/plan.json"
        build = self.cli("build", "--plan", path, "--expected-revision", self.sha, "--out", "rig-build")
        self.assertEqual(build.returncode, 0, build.stdout + build.stderr)
        (self.root / "needs.json").write_text(json.dumps({"plan": {"result": "success"},
            "build": {"result": "success"}, "shard": {"result": "skipped"}}))
        result = self.cli("verify", "--plan", path, "--expected-revision", self.sha,
                          "--needs", "needs.json", "--evidence", ".")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("coverage command", result.stdout)
        self.assertNotIn("area/ladder graders satisfied", result.stdout)
        result = self.verify(path)
        self.assertNotEqual(result.returncode, 0, "coverage-only evidence is never a full release rig")

    def test_complete_actual_reports_and_build_bind_one_source_and_handoff(self):
        path = self.complete()
        result = self.verify(path, handoff="rig-handoff")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        verify = self.cli("verify-handoff", "--handoff", "rig-handoff", "--expected-revision", self.sha)
        self.assertEqual(verify.returncode, 0, verify.stdout + verify.stderr)
        self.assertIn("4 case outcomes", verify.stdout)
        build = json.loads((self.root / "rig-build/build.json").read_text())
        self.assertEqual(build["sourceRevision"], self.sha)
        self.assertEqual(build["planDigest"], hashlib.sha256(path.read_bytes()).hexdigest())

    def test_empty_partial_skipped_duplicate_and_failed_raw_results_cannot_pass(self):
        path = self.prepared()
        for mode in ("empty", "partial", "skipped", "duplicate", "failed"):
            with self.subTest(mode=mode):
                result = self.cli("shard", "--plan", path, "--expected-revision", self.sha,
                                  "--shard", "ladder", "--out", "bad-" + mode, mode=mode)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn("unrecognized arguments", result.stderr)

    def test_missing_partial_stale_or_tampered_handoff_cannot_satisfy_full_gate(self):
        path = self.complete()
        for kind in ("missing", "partial", "stale", "stale-shard", "tampered", "wrong-plan", "empty-rehashed", "failed-command"):
            with self.subTest(kind=kind):
                result = self.verify(path, handoff="handoff-" + kind)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                directory = self.root / ("handoff-" + kind)
                if kind == "missing":
                    (directory / "rig-results/ladder/receipt.json").unlink()
                elif kind == "partial":
                    (directory / "rig-results/shell/results.json").write_text("[]")
                elif kind == "stale":
                    file = directory / "rig-build/build.json"
                    value = json.loads(file.read_text()); value["sourceRevision"] = "0" * 40
                    file.write_text(json.dumps(value))
                elif kind == "wrong-plan":
                    file = directory / "plan.json"
                    value = json.loads(file.read_text()); value["shards"] = ["ladder"]
                    file.write_text(json.dumps(value))
                elif kind in ("stale-shard", "failed-command", "empty-rehashed"):
                    file = directory / "rig-results/ladder/receipt.json"
                    value = json.loads(file.read_text())
                    if kind == "stale-shard": value["sourceRevision"] = "0" * 40
                    elif kind == "failed-command": value["commands"][0]["exitCode"] = 1
                    else:
                        report = directory / "rig-results/ladder/results.json"
                        report.write_text("[]")
                        value["resultsDigest"] = hashlib.sha256(report.read_bytes()).hexdigest()
                    file.write_text(json.dumps(value))
                else:
                    (directory / "rig-results/ladder/results.json").write_text("[{\"path\":\"invented\",\"status\":\"pass\"}]")
                result = self.cli("verify-handoff", "--handoff", directory, "--expected-revision", self.sha)
                self.assertNotEqual(result.returncode, 0, kind)

    def test_failed_cancelled_skipped_missing_real_needs_and_wrong_binary_fail(self):
        path = self.complete()
        for job in ("plan", "build", "shard"):
            for status in ("failure", "cancelled", "skipped", "missing"):
                with self.subTest(job=job, status=status):
                    needs = {name: {"result": "success"} for name in ("plan", "build", "shard")}
                    if status == "missing": del needs[job]
                    else: needs[job]["result"] = status
                    (self.root / "needs.json").write_text(json.dumps(needs))
                    result = self.verify(path)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("real job result", result.stdout + result.stderr)
        (self.root / "needs.json").write_text(json.dumps({job: {"result": "success"} for job in ("plan", "build", "shard")}))
        (self.root / "target/rig/debug/xtask").write_text("tampered")
        result = self.verify(path)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("binary", result.stdout + result.stderr)

    def test_coverage_failure_and_old_checkout_do_not_produce_successful_builds(self):
        path = self.prepared()
        result = self.cli("build", "--plan", path, "--expected-revision", self.sha, "--out", "bad-build", mode="coverage-fail")
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("unrecognized arguments", result.stderr)
        result = self.cli("build", "--plan", path, "--expected-revision", "0" * 40, "--out", "old-build")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("sourceRevision", result.stdout + result.stderr)

    def test_rig_has_only_hosted_read_only_bounded_build_paths(self):
        text = Path(WORKFLOW).read_text()
        for forbidden in ("blacksmith", "stickydisk", "sccache", "continue-on-error", "secrets."):
            self.assertNotIn(forbidden, text.lower())
        self.assertNotRegex(text, r"(?m)^\s+[\w-]+: write\s*$")
        self.assertIn("runs-on: ubuntu-24.04", text)
        self.assertIn("persist-credentials: false", text)


if __name__ == "__main__":
    unittest.main()
