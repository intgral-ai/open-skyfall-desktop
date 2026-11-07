#!/usr/bin/env python3
"""Self-test for affected.py. Run: python3 -I scripts/ci/test_affected.py (from the repo root).

Uses a small synthetic workspace, then the real one (`cargo metadata --no-deps`).
"""
import copy
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import affected  # noqa: E402

R = "/ws"


def pkg(name, dir_, deps=()):
    return {
        "name": name,
        "manifest_path": f"{R}/{dir_}/Cargo.toml",
        "dependencies": [{"name": d, "path": f"{R}/{p}"} for d, p in deps] + [{"name": "serde"}],
    }


SYNTH = {
    "workspace_root": R,
    "packages": [
        pkg("base", "crates/base"),
        pkg("mid", "crates/mid", [("base", "crates/base")]),
        pkg("top", "apps/top", [("mid", "crates/mid")]),
        pkg("alone", "crates/alone"),
    ],
}


def windows_metadata(root):
    meta = copy.deepcopy(SYNTH)
    meta["workspace_root"] = root
    prefix = root.rstrip("\\/")
    for p in meta["packages"]:
        p["manifest_path"] = prefix + "\\" + p["manifest_path"][len(R) + 1:]
        for d in p["dependencies"]:
            if "path" in d:
                d["path"] = prefix.replace("\\", "/") + "/" + d["path"][len(R) + 1:].replace("/", "\\")
    return meta


class Synthetic(unittest.TestCase):
    def test_leaf_selects_only_itself(self):
        self.assertEqual(affected.select(SYNTH, ["apps/top/src/main.rs"]), (False, ["top"]))

    def test_reverse_dependencies_are_added(self):
        self.assertEqual(affected.select(SYNTH, ["crates/base/src/lib.rs"]), (False, ["base", "mid", "top"]))

    def test_unrelated_package_is_not_selected(self):
        full, pk = affected.select(SYNTH, ["crates/mid/Cargo.toml"])
        self.assertEqual((full, pk), (False, ["mid", "top"]))

    def test_full_triggers(self):
        for p in ["Cargo.lock", "Cargo.toml", ".cargo/config.toml", ".config/nextest.toml", "rust-toolchain.toml",
                  ".github/workflows/ci.yml", "vendor/x/lib.rs", "tests/oracle/a.json", "assets/a.png",
                  "scripts/ci/affected.py", "unknown.txt"]:
            self.assertEqual(affected.select(SYNTH, ["crates/alone/a.rs", p]), (True, []), p)

    def test_docs_only_selects_nothing(self):
        self.assertEqual(affected.select(SYNTH, ["docs/book/a.md", "README.md", "packaging/x.yml"]), (False, []))

    def test_no_code_paths_select_nothing(self):
        # What CI reads as "no code changed", on pull requests and pushes to main alike.
        for p in [".github/workflows/docker.yml", ".github/ISSUE_TEMPLATE/bug.yml", "scripts/claim/claim.js",
                  "scripts/triage/triage.test.js", "vendor/README.md", "SECURITY.md"]:
            self.assertEqual(affected.select(SYNTH, [p]), (False, []), p)

    def test_windows_metadata_selects_packages_and_preserves_triggers(self):
        for root in [r"C:\work space\solidcraft", r"\\server\shared work\solidcraft"]:
            for trailing in ["", "\\", "/"]:
                with self.subTest(root=root, trailing=trailing):
                    meta = windows_metadata(root + trailing)
                    self.assertEqual(affected.select(meta, ["apps/top/src/main.rs"]), (False, ["top"]))
                    self.assertEqual(affected.select(meta, ["crates/base/src/lib.rs"]), (False, ["base", "mid", "top"]))
                    self.assertEqual(affected.select(meta, ["docs/book/a.md", "README.md"]), (False, []))
                    for path in ["Cargo.lock", ".github/workflows/ci.yml", "unknown.txt"]:
                        self.assertEqual(affected.select(meta, [path]), (True, []), path)

    def test_windows_metadata_rejects_a_manifest_in_a_sibling_workspace(self):
        for root in [r"C:\work space\solidcraft", r"\\server\shared work\solidcraft"]:
            with self.subTest(root=root):
                meta = windows_metadata(root + "\\")
                sibling = root.replace("\\", "/") + "-sibling/crates/base/Cargo.toml"
                meta["packages"][0]["manifest_path"] = sibling
                with self.assertRaises(AssertionError):
                    affected.select(meta, ["crates/base/src/lib.rs"])


class MonorepoSelection(unittest.TestCase):
    def setUp(self):
        self.assertIsNotNone(importlib.util.find_spec("plan"), "missing monorepo applicability planner")
        import plan
        self.planner = plan
        self.meta = {"workspace_root": R, "packages": [
            pkg("solidcraft-engine", "crates/engine"),
            pkg("solidcraft-ui-egui", "crates/ui-egui", [("solidcraft-engine", "crates/engine")]),
            pkg("solidcraft", "apps/solidcraft", [("solidcraft-ui-egui", "crates/ui-egui")]),
            pkg("xtask", "xtask"),
        ]}
        self.inventory = dict(contractPresent=True, runtimePresent=True, runtimePayloadRequested=False,
                              artifactVerifierPresent=True, missingContractInputs=[], missingRuntimeInputs=[])

    def select(self, *paths, **options):
        args = dict(source_revision="a" * 40, event_name="pull_request", base_ref="develop",
                    head_ref="chore/example", full=False, release_required=False, diff_complete=True)
        args.update(options)
        return self.planner.make_plan(self.meta, list(paths), self.inventory, **args)

    def decision(self, result, job, decision, reason, missing=()):
        self.assertEqual(result["jobs"][job], dict(decision=decision, reason=reason, missingInputs=list(missing)))

    def absent(self):
        with tempfile.TemporaryDirectory() as directory:
            self.inventory = self.planner.detect_inventory(Path(directory), [])

    def test_docs_run_repository_and_pr_policy_not_native(self):
        result = self.select("docs/book/a.md")
        self.decision(result, "repo", "run", "always")
        self.decision(result, "pr-policy", "run", "always")
        self.assertEqual(result["rustPackages"], [])
        for job in ("rust", "wasm", "contract-rust", "runtime", "contract-ts", "integration", "packages"):
            self.decision(result, job, "not_applicable", "no_relevant_change")
        self.assertEqual(set(result), {"version", "sourceRevision", "eventName", "baseRef", "headRef", "scope",
                                       "releaseRequired", "rustFull", "rustPackages", "jobs", "openObligations"})
        self.assertEqual((result["version"], result["sourceRevision"], result["scope"]), (1, "a" * 40, "affected"))

    def test_pr_policy_does_not_run_on_ordinary_push(self):
        self.decision(self.select(event_name="push"), "pr-policy", "not_applicable", "no_relevant_change")
        self.decision(self.select(event_name="merge_group"), "pr-policy", "run", "always")

    def test_engine_selects_reverse_dependents_and_wasm(self):
        result = self.select("crates/engine/src/lib.rs")
        self.assertEqual(result["rustPackages"], ["solidcraft", "solidcraft-engine", "solidcraft-ui-egui"])
        self.assertFalse(result["rustFull"])
        for job in ("rust", "wasm"):
            self.decision(result, job, "run", "selected_change")

    def test_embedded_document_keeps_rust_owner(self):
        self.assertIn("solidcraft-engine", self.select("NOTICE")["rustPackages"])

    def test_host_seams_and_shared_schema_select_all_contract_consumers(self):
        for path in ("host_ai.rs", "host_ai/client.rs", "mastra_protocol.rs", "runtime_client.rs", "process.rs"):
            with self.subTest(path=path):
                result = self.select("crates/ui-egui/src/agent_chat/" + path)
                for job in ("rust", "wasm", "contract-rust", "runtime", "contract-ts", "integration"):
                    self.decision(result, job, "run", "selected_change")
        for path in ("schema/mastra-runtime/v1/fixtures/a.json", "docs/mastra-runtime.md"):
            result = self.select(path)
            self.assertIn("solidcraft-ui-egui", result["rustPackages"])
            for job in ("contract-rust", "runtime", "contract-ts", "integration"):
                self.decision(result, job, "run", "selected_change")

    def test_runtime_node_pins_and_locks_select_both_languages(self):
        for path in ("packages/mastra-runtime/src/main.ts", "package.json", "package-lock.json", "pnpm-lock.yaml",
                     "pnpm-workspace.yaml", "yarn.lock", "bun.lock", ".node-version", ".nvmrc"):
            with self.subTest(path=path):
                result = self.select(path)
                for job in ("contract-rust", "runtime", "contract-ts", "integration", "artifact-policy"):
                    self.decision(result, job, "run", "selected_change")

    def test_shared_rust_and_ci_builds_select_every_existing_consumer(self):
        for path in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo/config.toml", ".config/nextest.toml",
                     ".github/workflows/docker.yml", "scripts/ci/plan.py", "scripts/build-runtime.sh"):
            with self.subTest(path=path):
                result = self.select(path)
                self.assertTrue(result["rustFull"])
                self.assertEqual(result["rustPackages"], ["solidcraft", "solidcraft-engine", "solidcraft-ui-egui", "xtask"])
                for job in ("rust", "wasm", "contract-rust", "runtime", "contract-ts", "integration", "artifact-policy"):
                    self.decision(result, job, "run", "selected_change")

    def test_full_unknown_and_incomplete_diff_are_conservative(self):
        for paths, options in (([], {"full": True}), (["unowned/input.bin"], {}),
                               (["unowned/input.bin", "Cargo.lock"], {}),
                               (["docs/a.md"], {"diff_complete": False}), ([], {"diff_complete": False})):
            result = self.select(*paths, **options)
            self.assertEqual(result["scope"], "full")
            for job in ("rust", "wasm", "contract-rust", "runtime", "contract-ts", "integration", "artifact-policy"):
                self.decision(result, job, "run", "full_validation")

    def test_schedule_selects_full_existing_consumers(self):
        result = self.select(event_name="schedule")
        self.assertEqual(result["scope"], "full")
        self.decision(result, "rust", "run", "full_validation")
        self.decision(result, "runtime", "run", "full_validation")

    def test_host_foundation_records_absent_consumers_without_ts_success(self):
        self.absent()
        result = self.select("crates/engine/src/lib.rs")
        self.decision(result, "runtime", "not_applicable", "runtime_not_implemented_host_foundation")
        self.decision(result, "contract-rust", "not_applicable", "contract_not_implemented_host_foundation")
        self.decision(result, "artifact-policy", "not_applicable", "verifier_not_implemented_host_foundation")
        self.assertEqual(result["openObligations"], ["artifact-policy", "contract-rust", "contract-ts", "integration", "runtime"])
        self.assertNotEqual(result["jobs"]["contract-ts"]["decision"], "run")
        self.decision(self.select("Cargo.lock"), "rust", "run", "selected_change")

    def test_contract_foundation_can_run_without_future_runtime(self):
        self.inventory.update(runtimePresent=False, artifactVerifierPresent=False,
                              missingRuntimeInputs=["packages/mastra-runtime/package.json"])
        result = self.select("schema/mastra-runtime/v1/envelope.json")
        self.decision(result, "contract-rust", "run", "selected_change")
        for job in ("runtime", "contract-ts", "integration"):
            self.decision(result, job, "not_applicable", "runtime_not_implemented_host_foundation")

    def test_changed_missing_contract_is_blocked_even_for_host_foundation(self):
        self.absent()
        for path in ("schema/mastra-runtime/v1/manifest.json", "docs/mastra-runtime.md",
                     "crates/ui-egui/src/agent_chat/runtime_client.rs"):
            self.decision(self.select(path), "contract-rust", "blocked", "missing_required_consumer",
                          ["crates/ui-egui/src/agent_chat/mastra_protocol.rs", "crates/ui-egui/src/agent_chat/runtime_client.rs",
                           "crates/ui-egui/src/agent_chat/tests.rs", "schema/mastra-runtime/v1/manifest.json"])

    def test_runtime_requests_and_deletions_cannot_be_foundation_skips(self):
        self.absent()
        for paths in (("packages/mastra-runtime/package.json",), (".node-version",),
                      ("packages/mastra-runtime/src/old.ts", "docs/new.md")):
            result = self.select(*paths)
            self.decision(result, "runtime", "blocked", "missing_required_consumer",
                          [".node-version", "package-lock.json", "package.json", "packages/mastra-runtime/package.json",
                           "packages/mastra-runtime/src/main.ts", "packages/mastra-runtime/tsconfig.json"])
            self.assertEqual(result["jobs"]["integration"]["decision"], "blocked")
        self.inventory["runtimePayloadRequested"] = True
        self.assertEqual(self.select("docs/a.md")["jobs"]["runtime"]["decision"], "blocked")

    def test_both_rename_names_are_used_even_if_target_is_documentation(self):
        result = self.select("crates/engine/src/old.rs", "docs/old.md")
        self.assertIn("solidcraft-engine", result["rustPackages"])
        self.absent()
        self.assertEqual(self.select("schema/mastra-runtime/v1/old.json", "docs/new.md")
                         ["jobs"]["contract-rust"]["decision"], "blocked")

    def test_missing_inputs_win_over_release_and_full_reasons(self):
        self.inventory["missingRuntimeInputs"] = [".node-version"]
        self.decision(self.select(full=True, release_required=True), "runtime", "blocked",
                      "missing_required_consumer", [".node-version"])
        self.inventory["missingRuntimeInputs"] = []
        result = self.select(full=True, release_required=True)
        self.assertTrue(result["releaseRequired"])
        for job in ("rust", "wasm", "contract-rust", "runtime", "contract-ts", "integration", "artifact-policy", "packages"):
            self.decision(result, job, "run", "release_required")
        self.absent()
        for job in ("contract-rust", "runtime", "contract-ts", "integration", "artifact-policy", "packages"):
            self.assertEqual(self.select(release_required=True)["jobs"][job]["decision"], "blocked")

    def test_known_missing_contract_inputs_block_existing_consumers_on_docs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.runtime_checkout(root)
            self.assertTrue(self.planner.detect_inventory(root, [])["contractPresent"])
            (root / "schema/mastra-runtime/v1/manifest.json").unlink()
            self.inventory = self.planner.detect_inventory(root, ["docs/a.md"])
            self.assertFalse(self.inventory["contractPresent"])
            self.assertTrue(self.inventory["runtimePresent"])
            self.assertFalse(self.inventory["runtimePayloadRequested"])
            self.assertEqual(self.inventory["missingContractInputs"], ["schema/mastra-runtime/v1/manifest.json"])
            result = self.select("docs/a.md")
            for job in ("contract-rust", "contract-ts", "integration"):
                self.decision(result, job, "blocked", "missing_required_consumer",
                              ["schema/mastra-runtime/v1/manifest.json"])

    def test_deleted_contract_inputs_block_complete_runtime_consumers(self):
        for path in ("crates/ui-egui/src/agent_chat/tests.rs", "schema/mastra-runtime/v1/manifest.json",
                     "crates/ui-egui/src/agent_chat/mastra_protocol.rs", "crates/ui-egui/src/agent_chat/runtime_client.rs"):
            with self.subTest(path=path), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                self.runtime_checkout(root)
                before = self.planner.detect_inventory(root, [])
                self.assertTrue(before["contractPresent"])
                self.assertTrue(before["runtimePresent"])
                (root / path).unlink()
                self.inventory = self.planner.detect_inventory(root, [path])
                self.assertFalse(self.inventory["contractPresent"])
                self.assertTrue(self.inventory["runtimePresent"])
                self.assertFalse(self.inventory["runtimePayloadRequested"])
                self.assertEqual(self.inventory["missingContractInputs"], [path])
                result = self.select(path)
                for job in ("contract-rust", "contract-ts", "integration"):
                    self.decision(result, job, "blocked", "missing_required_consumer", [path])
                    self.assertNotIn(job, result["openObligations"])
                self.decision(result, "runtime", "run", "selected_change")

    def test_deleted_contract_inputs_block_with_partial_runtime(self):
        for path in ("crates/ui-egui/src/agent_chat/tests.rs", "schema/mastra-runtime/v1/manifest.json",
                     "crates/ui-egui/src/agent_chat/mastra_protocol.rs", "crates/ui-egui/src/agent_chat/runtime_client.rs"):
            with self.subTest(path=path), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                self.runtime_checkout(root)
                self.assertTrue(self.planner.detect_inventory(root, [])["contractPresent"])
                (root / path).unlink()
                (root / ".node-version").unlink()
                self.inventory = self.planner.detect_inventory(root, [path])
                self.assertFalse(self.inventory["contractPresent"])
                self.assertFalse(self.inventory["runtimePresent"])
                self.assertTrue(self.inventory["runtimePayloadRequested"])
                self.assertEqual(self.inventory["missingContractInputs"], [path])
                self.assertEqual(self.inventory["missingRuntimeInputs"], [".node-version"])
                result = self.select(path)
                self.decision(result, "contract-rust", "blocked", "missing_required_consumer", [path])
                self.decision(result, "runtime", "blocked", "missing_required_consumer", [".node-version"])
                for job in ("contract-ts", "integration"):
                    self.decision(result, job, "blocked", "missing_required_consumer", [".node-version", path])

    def test_deleted_contract_inputs_block_without_future_runtime(self):
        for path in ("crates/ui-egui/src/agent_chat/tests.rs", "schema/mastra-runtime/v1/manifest.json",
                     "crates/ui-egui/src/agent_chat/mastra_protocol.rs", "crates/ui-egui/src/agent_chat/runtime_client.rs"):
            with self.subTest(path=path), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                self.contract_checkout(root)
                before = self.planner.detect_inventory(root, [])
                self.assertTrue(before["contractPresent"])
                self.assertFalse(before["runtimePresent"])
                (root / path).unlink()
                self.inventory = self.planner.detect_inventory(root, [path])
                self.assertFalse(self.inventory["contractPresent"])
                self.assertFalse(self.inventory["runtimePresent"])
                self.assertFalse(self.inventory["runtimePayloadRequested"])
                self.assertEqual(self.inventory["missingContractInputs"], [path])
                result = self.select(path)
                self.decision(result, "contract-rust", "blocked", "missing_required_consumer", [path])
                self.assertNotIn("contract-rust", result["openObligations"])
                for job in ("runtime", "contract-ts", "integration"):
                    self.decision(result, job, "not_applicable", "runtime_not_implemented_host_foundation")
                    self.assertIn(job, result["openObligations"])

    def test_unrelated_precontract_foundation_keeps_real_inventory_obligations(self):
        with tempfile.TemporaryDirectory() as directory:
            self.inventory = self.planner.detect_inventory(Path(directory), ["crates/engine/src/lib.rs"])
            self.assertFalse(self.inventory["contractPresent"])
            self.assertFalse(self.inventory["runtimePresent"])
            self.assertFalse(self.inventory["runtimePayloadRequested"])
            result = self.select("crates/engine/src/lib.rs")
            self.decision(result, "contract-rust", "not_applicable", "contract_not_implemented_host_foundation")
            for job in ("runtime", "contract-ts", "integration"):
                self.decision(result, job, "not_applicable", "runtime_not_implemented_host_foundation")
            self.assertEqual(result["openObligations"], ["artifact-policy", "contract-rust", "contract-ts", "integration", "runtime"])

    def test_legacy_chat_tests_change_does_not_require_future_contract(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = "crates/ui-egui/src/agent_chat/tests.rs"
            self.write(root, path, "#[test]\nfn existing_chat_history_test() {}")
            self.inventory = self.planner.detect_inventory(root, [path])
            self.assertFalse(self.inventory["contractPresent"])
            self.assertFalse(self.inventory["runtimePresent"])
            self.assertFalse(self.inventory["runtimePayloadRequested"])
            self.assertEqual(self.inventory["missingContractInputs"], [
                "crates/ui-egui/src/agent_chat/mastra_protocol.rs", "crates/ui-egui/src/agent_chat/runtime_client.rs",
                "crates/ui-egui/src/agent_chat/tests.rs", "schema/mastra-runtime/v1/manifest.json"])
            result = self.select(path)
            self.assertEqual(result["rustPackages"], ["solidcraft", "solidcraft-ui-egui"])
            self.decision(result, "rust", "run", "selected_change")
            self.decision(result, "contract-rust", "not_applicable", "contract_not_implemented_host_foundation")
            for job in ("runtime", "contract-ts", "integration"):
                self.decision(result, job, "not_applicable", "runtime_not_implemented_host_foundation")
            self.assertEqual(result["openObligations"], ["artifact-policy", "contract-rust", "contract-ts", "integration", "runtime"])

    def test_partial_contract_missing_input_blocks_without_future_runtime_on_docs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.contract_checkout(root)
            self.assertTrue(self.planner.detect_inventory(root, [])["contractPresent"])
            path = "crates/ui-egui/src/agent_chat/tests.rs"
            (root / path).unlink()
            self.inventory = self.planner.detect_inventory(root, ["docs/a.md"])
            self.assertFalse(self.inventory["contractPresent"])
            self.assertFalse(self.inventory["runtimePresent"])
            self.assertFalse(self.inventory["runtimePayloadRequested"])
            self.assertEqual(self.inventory["missingContractInputs"], [path])
            result = self.select("docs/a.md")
            self.decision(result, "contract-rust", "blocked", "missing_required_consumer", [path])
            for job in ("runtime", "contract-ts", "integration"):
                self.decision(result, job, "not_applicable", "runtime_not_implemented_host_foundation")
                self.assertIn(job, result["openObligations"])

    def test_policy_and_packaging_inputs_select_real_verifier(self):
        for path in ("docs/runtime-policy.md", "xtask/src/runtime_inventory.rs", "packaging/build.sh", "AGENTS.md"):
            self.decision(self.select(path), "artifact-policy", "run", "selected_change")
        self.decision(self.select("packaging/build.sh"), "packages", "run", "selected_change")

    def test_known_policy_and_utility_paths_do_not_require_installed_runtime(self):
        self.absent()
        self.inventory["artifactVerifierPresent"] = True
        for path in ("packaging/mastra/policy.json", "packaging/mastra/source-records.json",
                     "packaging/tests/nightly_test.py"):
            with self.subTest(path=path):
                result = self.select(path)
                self.decision(result, "repo", "run", "always")
                self.decision(result, "artifact-policy", "run", "selected_change")
                self.decision(result, "packages", "not_applicable", "no_relevant_change")
                self.decision(result, "runtime", "not_applicable", "runtime_not_implemented_host_foundation")

    def test_real_and_unknown_package_inputs_stay_conservative(self):
        self.absent()
        self.inventory["artifactVerifierPresent"] = True
        for path in ("packaging/build.sh", "packaging/new-input", "packaging/tests/new_test.py"):
            with self.subTest(path=path):
                result = self.select(path)
                self.assertEqual(result["jobs"]["packages"]["decision"], "blocked")
                self.assertEqual(result["jobs"]["artifact-policy"]["decision"], "run")
        for options in ({"release_required": True}, {"diff_complete": False}):
            result = self.select("packaging/mastra/policy.json", **options)
            self.assertEqual(result["jobs"]["artifact-policy"]["decision"], "run")
            self.assertEqual(result["jobs"]["packages"]["decision"], "blocked")
            if not options.get("diff_complete", True):
                self.assertEqual(result["scope"], "full")
                self.assertTrue(result["rustFull"])

    def test_invalid_revision_and_missing_inventory_are_rejected(self):
        for revision in ("main", "A" * 40, "a" * 39):
            with self.assertRaises(ValueError):
                self.select(source_revision=revision)
        del self.inventory["runtimePresent"]
        with self.assertRaises((KeyError, ValueError)):
            self.select()

    def write(self, root, path, text="fixture\n"):
        target = root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8")

    def contract_checkout(self, root):
        for path in ("schema/mastra-runtime/v1/manifest.json", "crates/ui-egui/src/agent_chat/mastra_protocol.rs",
                     "crates/ui-egui/src/agent_chat/runtime_client.rs"):
            self.write(root, path, "{}")
        self.write(root, "crates/ui-egui/src/agent_chat/tests.rs", "\n".join(
            f"#[test]\nfn mastra_{name}_fixture() {{}}" for name in ("values", "codec", "bundle", "state", "retirement", "peer")))

    def runtime_checkout(self, root):
        self.contract_checkout(root)
        self.write(root, "package.json", json.dumps({"private": True, "workspaces": ["packages/mastra-runtime"],
                                                    "packageManager": "npm@11.0.0"}))
        self.write(root, "packages/mastra-runtime/package.json", json.dumps({
            "name": "@intgral-ai/solidcraft-mastra-runtime", "private": True,
            "scripts": {"typecheck": "tsc --noEmit", "test": "node --test", "build": "tsc"}}))
        for path in ("packages/mastra-runtime/src/main.ts", "packages/mastra-runtime/tsconfig.json",
                     "package-lock.json", ".node-version"):
            self.write(root, path, "24.1.0" if path == ".node-version" else "{}")

    def test_inventory_does_not_mistake_directories_for_consumers(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "schema/mastra-runtime/v1").mkdir(parents=True)
            (root / "packages/mastra-runtime").mkdir(parents=True)
            inventory = self.planner.detect_inventory(root, [])
            self.assertFalse(inventory["contractPresent"])
            self.assertFalse(inventory["runtimePresent"])
            self.assertTrue(inventory["runtimePayloadRequested"])
            self.assertIn("schema/mastra-runtime/v1/manifest.json", inventory["missingContractInputs"])
            self.assertIn("packages/mastra-runtime/src/main.ts", inventory["missingRuntimeInputs"])

    def test_inventory_requires_real_contract_test_namespace_and_all_entrypoints(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.contract_checkout(root)
            self.assertTrue(self.planner.detect_inventory(root, [])["contractPresent"])
            path = root / "crates/ui-egui/src/agent_chat/tests.rs"
            path.write_text(path.read_text().replace("#[test]\nfn mastra_peer_fixture() {}", ""))
            self.write(root, "crates/ui-egui/src/agent_chat/mastra_peer.rs", "#[test]\nfn mastra_peer_fixture() {}")
            inventory = self.planner.detect_inventory(root, [])
            self.assertFalse(inventory["contractPresent"])
            self.assertEqual(inventory["missingContractInputs"], ["crates/ui-egui/src/agent_chat/tests.rs"])

    def test_changed_contract_test_area_selects_contract_when_present(self):
        self.decision(self.select("crates/ui-egui/src/agent_chat/tests.rs"), "contract-rust", "run", "selected_change")

    def test_inventory_runtime_requires_pins_and_build_entrypoints(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.runtime_checkout(root)
            self.assertTrue(self.planner.detect_inventory(root, [])["runtimePresent"])
            for path in (".node-version", "package-lock.json", "packages/mastra-runtime/src/main.ts"):
                old = (root / path).read_text()
                (root / path).unlink()
                inventory = self.planner.detect_inventory(root, [path])
                self.assertFalse(inventory["runtimePresent"])
                self.assertIn(path, inventory["missingRuntimeInputs"])
                self.write(root, path, old)
            self.write(root, "package.json", '{"packageManager":"npm@latest"}')
            self.assertIn("package.json", self.planner.detect_inventory(root, [])["missingRuntimeInputs"])
            self.write(root, "packages/mastra-runtime/package.json", '{"scripts":{"test":"node --test"}}')
            self.assertIn("packages/mastra-runtime/package.json", self.planner.detect_inventory(root, [])["missingRuntimeInputs"])

    def test_complete_runtime_is_not_a_payload_request_for_docs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.runtime_checkout(root)
            self.inventory = self.planner.detect_inventory(root, ["docs/a.md"])
            self.assertFalse(self.inventory["runtimePayloadRequested"])
            self.decision(self.select("docs/a.md"), "runtime", "not_applicable", "no_relevant_change")
            (root / ".node-version").unlink()
            self.assertTrue(self.planner.detect_inventory(root, ["docs/a.md"])["runtimePayloadRequested"])

    def test_inventory_requires_chosen_manager_lock_and_reports_deleted_pin(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.runtime_checkout(root)
            self.write(root, "package.json", json.dumps({"private": True, "workspaces": ["packages/mastra-runtime"],
                                                        "packageManager": "pnpm@10.0.0"}))
            inventory = self.planner.detect_inventory(root, [])
            self.assertFalse(inventory["runtimePresent"])
            self.assertIn("pnpm-lock.yaml", inventory["missingRuntimeInputs"])
            self.write(root, "pnpm-lock.yaml", "lockfileVersion: '9.0'")
            self.write(root, "pnpm-workspace.yaml", "packages:\n  - packages/mastra-runtime\n")
            self.assertTrue(self.planner.detect_inventory(root, [])["runtimePresent"])
            (root / ".node-version").rename(root / ".nvmrc")
            (root / ".nvmrc").unlink()
            self.assertIn(".nvmrc", self.planner.detect_inventory(root, [".nvmrc"])["missingRuntimeInputs"])

    def test_inventory_verifier_requires_reachable_actual_mode_entrypoints(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(root, "xtask/src/runtime_inventory.rs", 'fn run() { match mode {\n'
                       '"policy" => policy(), "host-only" => host_only(), "artifact" => artifact(), _ => fail() } }')
            self.assertFalse(self.planner.detect_inventory(root, [])["artifactVerifierPresent"])
            self.write(root, "xtask/src/main.rs", '// "policy" "host-only" "artifact"\nfn main() {}')
            self.assertFalse(self.planner.detect_inventory(root, [])["artifactVerifierPresent"])
            self.write(root, "xtask/src/main.rs", 'mod runtime_inventory;\nfn main() { runtime_inventory::run(); }')
            self.assertTrue(self.planner.detect_inventory(root, [])["artifactVerifierPresent"])
            self.write(root, "xtask/src/runtime_inventory.rs", 'fn run() { match mode {\n'
                       '"policy" => policy(), "host-only" => host_only(), _ => fail() } }')
            self.assertFalse(self.planner.detect_inventory(root, [])["artifactVerifierPresent"])

    def test_partial_pin_blocks_even_an_unrelated_doc_change(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(root, ".node-version", "24.1.0")
            self.inventory = self.planner.detect_inventory(root, ["docs/a.md"])
            result = self.select("docs/a.md")
            self.assertEqual(result["jobs"]["runtime"]["decision"], "blocked")
            self.assertIn("packages/mastra-runtime/package.json", result["jobs"]["runtime"]["missingInputs"])

    def test_cli_reads_nul_paths_and_emits_deterministic_sorted_json(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            meta = copy.deepcopy(self.meta)
            meta["workspace_root"] = str(root)
            for package in meta["packages"]:
                package["manifest_path"] = package["manifest_path"].replace(R, str(root), 1)
                for dependency in package["dependencies"]:
                    if "path" in dependency:
                        dependency["path"] = dependency["path"].replace(R, str(root), 1)
            self.write(root, "metadata.json", json.dumps(meta))
            (root / "paths").write_bytes(b"docs/with space.md\0crates/engine/src/old.rs\0docs/new.md\0")
            command = [sys.executable, "-I", self.planner.__file__, "--paths", str(root / "paths"),
                       "--metadata", str(root / "metadata.json"), "--source-revision", "a" * 40,
                       "--event", "pull_request", "--base-ref", "develop", "--head-ref", "chore/test",
                       "--out", str(root / "out.json")]
            first = subprocess.run(command, cwd=root, capture_output=True, text=True)
            self.assertEqual(first.returncode, 0, first.stderr)
            text = (root / "out.json").read_text()
            result = json.loads(text)
            self.assertIn("solidcraft-engine", result["rustPackages"])
            self.assertEqual(text, json.dumps(result, indent=2, sort_keys=True) + "\n")
            second = subprocess.run(command, cwd=root, capture_output=True, text=True)
            self.assertEqual(second.returncode, 0, second.stderr)
            self.assertEqual((root / "out.json").read_text(), text)
            release = subprocess.run(command + ["--release-required"], cwd=root, capture_output=True, text=True)
            self.assertEqual(release.returncode, 1, release.stderr)
            self.assertEqual(json.loads((root / "out.json").read_text())["jobs"]["runtime"]["decision"], "blocked")
            (root / "paths").unlink()
            self.assertNotEqual(subprocess.run(command, cwd=root, capture_output=True).returncode, 0)
            incomplete = subprocess.run(command + ["--diff-incomplete"], cwd=root, capture_output=True, text=True)
            self.assertEqual(incomplete.returncode, 0, incomplete.stderr)
            self.assertTrue(json.loads((root / "out.json").read_text())["rustFull"])


class ForkWorkflowContract(unittest.TestCase):
    """Protect required execution edges, not runner-success claims."""

    @classmethod
    def setUpClass(cls):
        cls.root = Path(__file__).resolve().parents[2]
        cls.workflow = (cls.root / ".github/workflows/ci.yml").read_text()

    def job(self, name):
        import re
        match = re.search(r"(?ms)^  " + re.escape(name) + r":\n(.*?)(?=^  [a-z][a-z0-9-]*:|\Z)", self.workflow)
        self.assertIsNotNone(match, f"missing workflow job {name}")
        return match[1]

    def test_drafts_and_labels_cannot_downgrade_selected_native_or_wasm(self):
        self.assertNotIn("pull_request.draft", self.workflow)
        self.assertNotIn("approved-for-ci", self.workflow)
        self.assertNotIn("quick-compile", self.workflow)
        self.assertIn("branches: [develop, main]", self.workflow)
        self.assertIn("merge_group:", self.workflow)
        self.assertIn("jobs.wasm.decision", self.job("wasm"))

    def test_ci_always_checks_real_needs_and_task_two_receipts(self):
        aggregate = self.job("ci")
        self.assertIn("name: ci\n", aggregate)
        self.assertIn("if: always()", aggregate)
        self.assertIn("toJSON(needs)", aggregate)
        self.assertIn("scripts/ci/main_merge.py aggregate", aggregate)
        self.assertNotIn("main_merge.py --base", self.workflow)
        self.assertNotIn("carried", self.workflow)

    def test_fork_native_uses_standard_runners_and_read_only_tokens(self):
        self.assertNotIn("blacksmith", self.workflow.lower())
        for path in sorted((self.root / ".github/workflows").glob("*.yml")):
            with self.subTest(workflow=path.name):
                text = path.read_text()
                self.assertNotIn("blacksmith", text.lower())
                self.assertNotIn("useblacksmith/stickydisk", text)
        self.assertNotRegex(self.workflow, r"(?m)^\s+[\w-]+: write\s*$")
        self.assertNotIn("secrets.", self.workflow)
        self.assertNotIn("pull_request_target", self.workflow)
        self.assertNotIn("--no-tests=pass", self.workflow)
        self.assertIn("runs-on: ubuntu-24.04", self.job("rust"))
        self.assertIn("runs-on: windows-latest", self.job("windows-fxc"))
        self.assertIn("WGPU_BACKEND: vulkan", self.job("rust"))
        self.assertIn('SOLIDCRAFT_REQUIRE_GPU: "1"', self.job("rust"))

    def test_exact_plan_and_required_consumers_have_evidence_edges(self):
        for job in ("repo", "pr-policy", "rust", "wasm"):
            self.assertIn("scripts/ci/main_merge.py execute", self.job(job))
            self.assertIn("selection-plan", self.job(job))
        for command in ("validation_mode:", "release_required:", "test_gate.py", "test_kernel_rig_plan.py"):
            self.assertIn(command, self.workflow)
        self.assertIn("Full baseline", self.job("property-tests"))
        self.assertIn("inputs.cross_compile", self.job("riscv64-gates"))
        self.assertIn("riscv64-gates", self.job("ci"))


class NativeBaselineWorkflow(unittest.TestCase):
    """Exercise the raw runner's exit boundary, separately from reviewed regression grading."""

    def setUp(self):
        self.root = Path(__file__).resolve().parents[2]
        path = self.root / ".github/workflows/native-baseline.yml"
        self.assertTrue(path.is_file(), "missing independently visible raw native baseline")
        self.workflow = path.read_text()

    def test_integration_schedule_and_manual_are_read_only_and_unmasked(self):
        self.assertIn("branches: [develop, main]", self.workflow)
        for trigger in ("schedule:", "workflow_dispatch:"):
            self.assertIn(trigger, self.workflow)
        for forbidden in ("continue-on-error", "--no-tests=pass", "secrets.", "pull_request_target", "|| true"):
            self.assertNotIn(forbidden, self.workflow)
        self.assertNotRegex(self.workflow, r"(?m)^\s+[\w-]+: write\s*$")
        self.assertIn("SOLIDCRAFT_GPU: software", self.workflow)
        self.assertIn("WGPU_BACKEND: vulkan", self.workflow)
        self.assertIn('SOLIDCRAFT_REQUIRE_GPU: "1"', self.workflow)
        self.assertIn('PROPTEST_CASES: "32"', self.workflow)
        self.assertIn('ATOMIC_SEEDS: "0..8"', self.workflow)

    def test_full_reusable_gate_and_raw_evidence_are_distinct(self):
        self.assertIn("uses: ./.github/workflows/ci.yml", self.workflow)
        self.assertIn("validation_mode: full", self.workflow)
        self.assertIn("release_required: false", self.workflow)
        self.assertIn("toJSON(needs)", self.workflow)
        self.assertIn("if: always()", self.workflow)
        for path in ("selection/", "evidence/raw-native/", "if-no-files-found: error"):
            self.assertIn(path, self.workflow)

    def raw_script(self, step="Run full raw native baseline"):
        import re
        match = re.search(r"(?ms)      - name: " + re.escape(step) + r"\n(?:        env:\n(?:          [^\n]*\n)+)?        run: \|\n(.*?)(?=^      - |\Z)", self.workflow)
        self.assertIsNotNone(match, "missing executable raw baseline step")
        lines = [line[10:] for line in match[1].splitlines()]
        self.assertEqual(lines[0], "python3 -I - <<'PY'")
        self.assertEqual(lines[-1], "PY")
        return "\n".join(lines[1:-1])

    def run_raw(self, *, status=0, outcome="passed", listing_status=0, revision=None, heavy=None):
        # This external Cargo fixture tests command/exit/report plumbing, not native CAD success.
        # The workflow executes the actual T4 adapter against real git and child processes.
        import gate
        import plan
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q", directory], check=True)
            subprocess.run(["git", "-C", directory, "-c", "user.name=Test", "-c", "user.email=test@example.invalid",
                            "commit", "-q", "--allow-empty", "-m", "fixture"], check=True)
            sha = subprocess.check_output(["git", "-C", directory, "rev-parse", "HEAD"], text=True).strip()
            (root / "scripts").mkdir()
            (root / "scripts/ci").symlink_to(self.root / "scripts/ci", target_is_directory=True)
            (root / "selection").mkdir()
            selection = plan.make_plan(SYNTH, [], plan.detect_inventory(root, []), source_revision=revision or sha,
                                       event_name="push", base_ref="develop", head_ref="develop", full=True,
                                       release_required=False, diff_complete=True)
            raw_plan = (json.dumps(selection, indent=2, sort_keys=True) + "\n").encode()
            (root / "selection/plan.json").write_bytes(raw_plan)
            known = sorted(gate.read_known_failures(gate.KNOWN_FAILING))[0]
            binary, case = known.split("::", 1) if outcome == "known" else ("fixture", "live")
            listing = {"rust-build-meta": {"target-directory": str(root / "target")}, "test-count": 1,
                       "rust-suites": {binary: {"binary-id": binary, "status": "listed", "testcases": {
                           case: {"ignored": False, "filter-match": {"status": "matches"}}}}}}
            if outcome == "empty":
                listing["test-count"] = 0
                listing["rust-suites"][binary]["testcases"] = {}
            tag = "<failure/>" if outcome in ("failed", "known") else "<skipped/>" if outcome == "skipped" else ""
            junit = f'<testsuites><testsuite name="{binary}"><testcase name="{case}">{tag}</testcase></testsuite></testsuites>'
            if outcome == "missing":
                junit = '<testsuites><testsuite name="fixture"><testcase name="other"/></testsuite></testsuites>'
            cargo = root / "cargo"
            cargo.write_text(f"#!{sys.executable}\nimport json, os, pathlib, sys\nargs=sys.argv[1:]\n"
                             f"if args[:2] == ['nextest', 'list']:\n print({json.dumps(listing)!r}); sys.exit({listing_status})\n"
                             "if args[:2] == ['nextest', 'run']:\n"
                             " p=pathlib.Path('target/nextest/ci/junit.xml'); p.parent.mkdir(parents=True, exist_ok=True)\n"
                             f" p.write_text({junit!r}); print(json.dumps({{k: os.environ.get(k) for k in "
                             f"('ACTION_MATRIX_SHARD', 'ACTION_MATRIX_RESULTS', 'ACTION_MATRIX_TIMINGS', 'ACTION_MATRIX_WORKDIR')}})); sys.exit({status})\n"
                             "if args[:2] == ['xtask', 'known-failing']: sys.exit(0)\n"
                             "sys.exit(91)\n")
            cargo.chmod(0o755)
            summary = root / "summary.md"
            script = self.raw_script("Run raw ignored matrix suite" if heavy else "Run full raw native baseline")
            run = subprocess.run([sys.executable, "-I", "-c", script], cwd=root, text=True,
                                 capture_output=True, env={**os.environ, "PATH": str(root) + os.pathsep + os.environ["PATH"],
                                                          "BASELINE_SUITE": heavy or "native",
                                                          "GITHUB_STEP_SUMMARY": str(summary)})
            evidence = root / ("evidence/raw-heavy" if heavy else "evidence/raw-native")
            result_path = evidence / "raw-result.json"
            result = json.loads(result_path.read_text()) if result_path.exists() else None
            if result:
                self.assertEqual(result["sourceRevision"], sha)
                self.assertEqual(result["planDigest"], gate.digest_for(gate.BoundPlan(raw_plan)))
                self.assertTrue((evidence / "selection.json").is_file())
                commands = result["commands"]
                if len(commands) > 1:
                    self.assertEqual(gate.nextest_configuration(commands[0]["argv"], "list"),
                                     gate.nextest_configuration(commands[1]["argv"], "run"))
                    if heavy:
                        self.assertIn("--archive-file", commands[1]["argv"])
                        self.assertIn("--run-ignored", commands[1]["argv"])
                        self.assertIn("only", commands[1]["argv"])
                        name = "action_matrix_determinism" if heavy == "determinism" else "action_matrix_shard"
                        self.assertEqual(commands[1]["argv"][commands[1]["argv"].index("-E") + 1],
                                         "binary(action_matrix) & test(=" + name + ")")
                        child_env = json.loads((evidence / "command-1.log").read_text())
                        if heavy != "determinism":
                            self.assertEqual(child_env["ACTION_MATRIX_SHARD"], heavy + "/16")
                        for key, filename in (("ACTION_MATRIX_RESULTS", "results.json"),
                                              ("ACTION_MATRIX_TIMINGS", "timings.jsonl"), ("ACTION_MATRIX_WORKDIR", "raw")):
                            self.assertEqual(child_env[key], str(evidence / filename))
                    else:
                        self.assertIn("all()", commands[1]["argv"])
                        self.assertIn("--workspace", commands[1]["argv"])
                    self.assertTrue((evidence / "junit.xml").is_file())
            return run, result, summary.read_text() if summary.exists() else ""

    def test_raw_pass_requires_nonempty_complete_real_reports(self):
        run, result, summary = self.run_raw()
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(result["passedIds"], ["fixture::live"])
        self.assertEqual(result["failedIds"], [])
        self.assertIn("Raw native baseline: PASS", summary)

    def test_known_and_unknown_failures_remain_raw_failures(self):
        for outcome in ("known", "failed"):
            with self.subTest(outcome=outcome):
                run, result, summary = self.run_raw(status=100, outcome=outcome)
                self.assertEqual(run.returncode, 100, run.stderr)
                self.assertEqual(result["commands"][1]["exitCode"], 100)
                self.assertEqual(len(result["failedIds"]), 1)
                self.assertEqual(bool(result["knownFailingIds"]), outcome == "known")
                self.assertIn("Raw native baseline: RED", summary)

    def test_listing_infrastructure_skipped_and_partial_evidence_fail_closed(self):
        for args, exit_code in (({"listing_status": 2}, 2), ({"status": 101}, 101),
                                ({"outcome": "skipped"}, 1), ({"outcome": "missing"}, 1), ({"outcome": "empty"}, 1)):
            with self.subTest(args=args):
                run, _, summary = self.run_raw(**args)
                self.assertEqual(run.returncode, exit_code, run.stderr)
                self.assertIn("Raw native baseline: RED", summary)

    def test_wrong_checkout_revision_cannot_execute(self):
        run, result, _ = self.run_raw(revision="b" * 40)
        self.assertNotEqual(run.returncode, 0)
        self.assertIsNone(result)
        self.assertIn("actual checkout SHA differs", run.stderr)

    def test_ignored_heavy_matrix_keeps_raw_exits_and_exact_selection(self):
        self.assertIn("shard: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, determinism]", self.workflow)
        self.assertIn("fail-fast: false", self.workflow)
        for suite in ("1", "16", "determinism"):
            for status, outcome in ((0, "passed"), (100, "failed"), (101, "passed"), (0, "missing")):
                with self.subTest(suite=suite, status=status, outcome=outcome):
                    run, result, summary = self.run_raw(status=status, outcome=outcome, heavy=suite)
                    self.assertEqual(run.returncode, status or (1 if outcome == "missing" else 0), run.stderr)
                    self.assertEqual(result["commands"][1]["exitCode"], status)
                    self.assertIn("Raw ignored matrix baseline:", summary)

    def test_conclusion_rejects_missing_failed_cancelled_and_skipped_jobs(self):
        script = self.raw_script("Report actual full and raw results")
        valid = {job: {"result": "success"} for job in ("regression", "raw-native", "raw-heavy")}
        cases = [(valid, 0), ({}, 1)]
        for job in valid:
            for status in ("failure", "cancelled", "skipped"):
                needs = copy.deepcopy(valid)
                needs[job]["result"] = status
                cases.append((needs, 1))
        with tempfile.TemporaryDirectory() as directory:
            for needs, status in cases:
                with self.subTest(needs=needs):
                    summary = Path(directory) / "summary.md"
                    run = subprocess.run([sys.executable, "-I", "-c", script], capture_output=True, text=True,
                                         env={**os.environ, "NEEDS": json.dumps(needs), "GITHUB_STEP_SUMMARY": str(summary)})
                    self.assertEqual(run.returncode, status, run.stderr)


class RealWorkspace(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.meta = affected.load_metadata(None)

    def test_render_change_selects_render_and_dependents_not_kernel(self):
        full, pk = affected.select(self.meta, ["crates/render/src/lib.rs"])
        self.assertFalse(full)
        self.assertIn("solidcraft-render", pk)
        self.assertGreater(len(pk), 1)
        self.assertNotIn("solidcraft-kernel", pk)

    def test_cargo_lock_selects_full(self):
        self.assertEqual(affected.select(self.meta, ["Cargo.lock"]), (True, []))

    def test_docs_only_selects_nothing(self):
        self.assertEqual(affected.select(self.meta, ["docs/book/src/dev/testing.md", "CONTRIBUTING.md"]), (False, []))

    def test_engine_test_data_selects_the_engine_and_its_dependents_not_the_kernel(self):
        full, pk = affected.select(self.meta, ["tests/hole_matrix/known_failures.txt"])
        self.assertFalse(full)
        self.assertIn("solidcraft-engine", pk)
        self.assertNotIn("solidcraft-kernel", pk)
        self.assertNotIn("solidcraft-render", pk)

    def test_oracle_cases_select_the_cli_and_xtask_not_the_kernel(self):
        full, pk = affected.select(self.meta, ["tests/oracle/box/recipe.json"])
        self.assertFalse(full)
        self.assertIn("solidcraft-cli", pk)
        self.assertIn("xtask", pk)
        self.assertNotIn("solidcraft-kernel", pk)

    def test_embedded_root_files_select_their_crate(self):
        for path, pkg in [("ATTRIBUTION.md", "solidcraft-ui-egui"), ("LICENSE-MIT", "solidcraft-engine"),
                          ("NOTICE", "solidcraft-engine"), ("THIRD-PARTY-LICENSES.txt", "solidcraft-engine"),
                          ("skills/solidcraft-modeling/SKILL.md", "solidcraft-mcp")]:
            full, pk = affected.select(self.meta, [path])
            self.assertFalse(full, path)
            self.assertIn(pkg, pk, path)
            self.assertNotIn("solidcraft-kernel", pk, path)

    def test_unmapped_test_data_selects_full(self):
        self.assertEqual(affected.select(self.meta, ["tests/tools/x.json"]), (True, []))

    def test_vendored_egui_wgpu_selects_the_ui_and_apps_not_the_kernel(self):
        full, pk = affected.select(self.meta, ["vendor/egui-wgpu/src/winit.rs"])
        self.assertFalse(full)
        self.assertIn("solidcraft-ui-egui", pk)
        self.assertIn("solidcraft", pk)
        self.assertNotIn("solidcraft-kernel", pk)
        self.assertNotIn("solidcraft-engine", pk)

    def test_vendor_readme_alone_selects_nothing(self):
        self.assertEqual(affected.select(self.meta, ["vendor/README.md"]), (False, []))

    def test_translation_selects_the_ui_and_xtask_not_the_kernel(self):
        full, pk = affected.select(self.meta, ["assets/locales/ja.json", "assets/locales/it.json"])
        self.assertFalse(full)
        self.assertIn("solidcraft-ui-egui", pk)
        self.assertIn("xtask", pk)
        self.assertNotIn("solidcraft-kernel", pk)
        self.assertNotIn("solidcraft-engine", pk)


if __name__ == "__main__":
    unittest.main()
