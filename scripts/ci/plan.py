#!/usr/bin/env python3
"""Explicit CI applicability; inventory is presence evidence, never test-success evidence.

Use git diff --no-renames --name-only -z BASE...HEAD for --paths so both rename
names and deletions survive. An unavailable comparison requires --diff-incomplete.
This utility does not install, execute or manufacture future consumers.
CLI exits 0 for an unblocked plan (not a test pass), 1 after writing a blocked
plan, or 2 for invalid/unavailable inputs without an explicit conservative mode.
"""
import argparse
import json
from pathlib import Path
import re
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
import affected  # noqa: E402

HOST = "crates/ui-egui/src/agent_chat/"
CONTRACT_INPUTS = ("schema/mastra-runtime/v1/manifest.json", HOST + "mastra_protocol.rs",
                   HOST + "runtime_client.rs", HOST + "tests.rs")
RUNTIME_INPUTS = ("package.json", "packages/mastra-runtime/package.json",
                  "packages/mastra-runtime/src/main.ts", "packages/mastra-runtime/tsconfig.json")
NODE_PINS = (".node-version", ".nvmrc")
NODE_LOCKS = ("package-lock.json", "pnpm-lock.yaml", "yarn.lock", "bun.lock", "bun.lockb")
NODE_FILES = (*NODE_PINS, *NODE_LOCKS, "package.json", "pnpm-workspace.yaml")
VERIFIER_ENTRY = "xtask/src/main.rs"
# Only these reviewed metadata/owning utility inputs are payload-free. Unknown
# packaging paths still require the real package consumers, never a prefix waiver.
PACKAGING_FOUNDATION = frozenset(("packaging/mastra/policy.json", "packaging/mastra/source-records.json",
                                  "packaging/tests/apprun_test.py", "packaging/tests/flatpak_test.py",
                                  "packaging/tests/nightly_test.py", "packaging/tests/windows_icon_test.py"))


def runtime_path(path):
    return path.startswith("packages/mastra-runtime/") or path in NODE_FILES


def contract_path(path):
    return (path.startswith("schema/mastra-runtime/") or path == "docs/mastra-runtime.md"
            or path.startswith(tuple(HOST + name for name in
                                     ("host_ai", "mastra_protocol", "runtime_client", "process"))))


def shared_path(path):
    return (path in affected.FULL_FILES or path.startswith("rust-toolchain")
            or path.startswith((".cargo/", ".config/", ".github/workflows/", ".github/actions/"))
            or (path.startswith("scripts/") and not path.startswith(("scripts/claim/", "scripts/triage/"))))


def policy_path(path):
    return (path in ("AGENTS.md", "CONTRIBUTING.md", "deny.toml", "ATTRIBUTION.md", "NOTICE")
            or path.startswith(("packaging/", "attribution/", "xtask/", "skills/solidcraft-ground-rules/"))
            or (path.startswith("docs/") and any(word in path for word in ("policy", "provenance", "licens", "rights"))))


def verifier_present(root):
    """Recognize #15's reachable xtask mode entry points, not a competing verifier CLI."""
    pending, seen = [root / VERIFIER_ENTRY], set()
    while pending:
        path = pending.pop()
        if path in seen or not path.is_file():
            continue
        seen.add(path)
        text = path.read_text(encoding="utf-8")
        text = re.sub(r"//[^\n]*|/\*.*?\*/", "", text, flags=re.S)
        if all(re.search('"' + mode + r'"\s*\)?\s*=>', text) for mode in ("policy", "host-only", "artifact")):
            return True
        for module in re.findall(r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;", text):
            directory = path.parent if path.name in ("main.rs", "mod.rs") else path.with_suffix("")
            pending.extend((directory / (module + ".rs"), directory / module / "mod.rs"))
    return False


def detect_inventory(root, paths):
    """Inspect checkout entry points/pins. Missing future code remains an obligation."""
    root = Path(root)

    def exists(path):
        return (root / path).is_file() and (root / path).stat().st_size > 0

    def manifest(path):
        try:
            data = json.loads((root / path).read_text(encoding="utf-8"))
            return data if isinstance(data, dict) else {}
        except (OSError, ValueError):
            return {}

    missing_contract = {path for path in CONTRACT_INPUTS if not exists(path)}
    if exists(HOST + "tests.rs"):
        tests = (root / (HOST + "tests.rs")).read_text(encoding="utf-8")
        tests = re.sub(r"//[^\n]*|/\*.*?\*/", "", tests, flags=re.S)
        if not all(re.search(r"#\[test\]\s*(?:#\[[^\n]*\]\s*)*fn\s+mastra_" + group + r"_\w+\s*\(", tests)
                   for group in ("values", "codec", "bundle", "state", "retirement", "peer")):
            missing_contract.add(HOST + "tests.rs")
    missing_runtime = {path for path in RUNTIME_INPUTS if not exists(path)}
    pins = [path for path in NODE_PINS if exists(path)]
    if not pins:
        missing_runtime.update([path for path in NODE_PINS if path in paths] or [NODE_PINS[0]])
    for path in pins:
        if not re.fullmatch(r"v?\d+\.\d+\.\d+\s*", (root / path).read_text(encoding="utf-8")):
            missing_runtime.add(path)
    workspace = manifest("package.json")
    manager = str(workspace.get("packageManager", "")).split("@", 1)[0]
    locks = {"npm": ("package-lock.json",), "pnpm": ("pnpm-lock.yaml",),
             "yarn": ("yarn.lock",), "bun": ("bun.lock", "bun.lockb")}.get(manager, (NODE_LOCKS[0],))
    if not any(exists(path) for path in locks):
        missing_runtime.add(locks[0])
    if manager == "pnpm" and not exists("pnpm-workspace.yaml"):
        missing_runtime.add("pnpm-workspace.yaml")
    workspaces = workspace.get("workspaces", [])
    if isinstance(workspaces, dict):
        workspaces = workspaces.get("packages", [])
    if (workspace.get("private") is not True or not isinstance(workspaces, list)
            or not any(path in workspaces for path in ("packages/mastra-runtime", "packages/*"))
            or not re.fullmatch(r"(?:npm|pnpm|yarn|bun)@\d+\.\d+\.\d+(?:\+[^\s]+)?", str(workspace.get("packageManager", "")))):
        missing_runtime.add("package.json")
    runtime = manifest("packages/mastra-runtime/package.json")
    scripts = runtime.get("scripts", {})
    if (runtime.get("name") != "@intgral-ai/solidcraft-mastra-runtime" or runtime.get("private") is not True
            or not isinstance(scripts, dict) or not all(isinstance(scripts.get(key), str) and scripts[key].strip()
                                                       for key in ("typecheck", "test", "build"))):
        missing_runtime.add("packages/mastra-runtime/package.json")
    partial_runtime = missing_runtime and ((root / "packages/mastra-runtime").exists()
                                          or any((root / path).exists() for path in NODE_FILES))
    payload = any(runtime_path(path) for path in paths) or bool(partial_runtime)
    return dict(contractPresent=not missing_contract, runtimePresent=not missing_runtime,
                runtimePayloadRequested=payload, artifactVerifierPresent=verifier_present(root),
                missingContractInputs=sorted(missing_contract), missingRuntimeInputs=sorted(missing_runtime))


def make_plan(meta: dict, paths: list[str], inventory: dict, *, source_revision: str,
              event_name: str, base_ref: str, head_ref: str, full: bool,
              release_required: bool, diff_complete: bool) -> dict:
    """Produce version 1 decisions. Blocked/missing beats release, full and path selection."""
    if not re.fullmatch(r"[0-9a-f]{40}", source_revision):
        raise ValueError("source_revision must be 40 lowercase hex characters")
    for key in ("contractPresent", "runtimePresent", "runtimePayloadRequested", "artifactVerifierPresent"):
        if type(inventory[key]) is not bool:
            raise ValueError(key + " must be a boolean")
    for key in ("missingContractInputs", "missingRuntimeInputs"):
        if not isinstance(inventory[key], list) or not all(isinstance(p, str) and p for p in inventory[key]):
            raise ValueError(key + " must be a list of paths")

    contract_missing = sorted(set(inventory["missingContractInputs"]))
    if not inventory["contractPresent"] and not contract_missing:
        contract_missing = list(sorted(CONTRACT_INPUTS))
    # A remaining contract entry point (including the checked Mastra test groups)
    # proves partial implementation. A legacy shared tests.rs alone does not.
    contract_required = (inventory["contractPresent"] or inventory["runtimePresent"]
                         or any(path not in contract_missing for path in CONTRACT_INPUTS))
    contract_change = (any(contract_path(path) for path in paths)
                       or (contract_required and HOST + "tests.rs" in paths))
    runtime_request = inventory["runtimePayloadRequested"] or any(runtime_path(path) for path in paths)
    shared = any(shared_path(path) for path in paths)
    policy = any(policy_path(path) for path in paths)
    packaging = any(path.startswith("packaging/") and (not diff_complete or path not in PACKAGING_FOUNDATION)
                    for path in paths)
    # Keep the existing Rust selector and its embedded/test-data exceptions as the
    # sole owner of Rust reverse dependencies. Map interface inputs to their host.
    rust_paths = [path for path in paths if not runtime_path(path)
                  and not path.startswith("schema/mastra-runtime/") and path != "docs/mastra-runtime.md"]
    rust_full, rust_packages = affected.select(meta, rust_paths)
    unknown = any(affected.select(meta, [path])[0] for path in rust_paths if not shared_path(path))
    full = full or event_name == "schedule" or not diff_complete or unknown
    rust_full = rust_full or shared or full or release_required
    if (contract_change or runtime_request) and not rust_full:
        _, rust_packages = affected.select(meta, rust_paths + [HOST + "mastra_protocol.rs"])
    if rust_full:
        rust_packages = sorted(p["name"] for p in meta["packages"])
    selected = full or shared or contract_change or runtime_request
    # Current consumers do not disappear when a required input goes missing.
    required_contract_missing = bool(contract_missing) and contract_required
    runtime_missing = sorted(set(inventory["missingRuntimeInputs"]))
    if not inventory["runtimePresent"] and not runtime_missing:
        runtime_missing = sorted((*RUNTIME_INPUTS, NODE_PINS[0], NODE_LOCKS[0]))
    verifier_missing = [] if inventory["artifactVerifierPresent"] else [VERIFIER_ENTRY]
    jobs, obligations = {}, []

    def decide(job, wanted, missing=(), absent_reason="no_relevant_change"):
        missing = sorted(set(missing))
        if wanted:
            decision = "blocked" if missing else "run"
            reason = ("missing_required_consumer" if missing else "release_required" if release_required
                      else "full_validation" if full else "selected_change")
        else:
            decision, reason = "not_applicable", absent_reason if missing else "no_relevant_change"
            if missing and absent_reason != "no_relevant_change":
                obligations.append(job)
            missing = []
        jobs[job] = dict(decision=decision, reason=reason, missingInputs=missing)

    jobs["repo"] = dict(decision="run", reason="always", missingInputs=[])
    pr = event_name in ("pull_request", "pull_request_target", "merge_group")
    jobs["pr-policy"] = dict(decision="run" if pr else "not_applicable",
                             reason="always" if pr else "no_relevant_change", missingInputs=[])
    decide("rust", bool(rust_packages) or rust_full)
    decide("wasm", bool(rust_packages) or rust_full)
    decide("contract-rust", release_required or contract_change or runtime_request
           or (inventory["contractPresent"] and selected) or required_contract_missing,
           contract_missing, "contract_not_implemented_host_foundation")
    runtime_wanted = release_required or runtime_request or (inventory["runtimePresent"] and (selected or bool(runtime_missing)))
    decide("runtime", runtime_wanted, runtime_missing, "runtime_not_implemented_host_foundation")
    for job in ("contract-ts", "integration"):
        decide(job, runtime_wanted or (inventory["runtimePresent"] and required_contract_missing), contract_missing + runtime_missing,
               "runtime_not_implemented_host_foundation" if runtime_missing else "contract_not_implemented_host_foundation")
    decide("artifact-policy", release_required or runtime_request
           or (inventory["artifactVerifierPresent"] and (selected or policy)), verifier_missing,
           "verifier_not_implemented_host_foundation")
    decide("packages", release_required or packaging, contract_missing + runtime_missing + verifier_missing)
    return dict(version=1, sourceRevision=source_revision, eventName=event_name,
                baseRef=base_ref, headRef=head_ref, scope="full" if full else "affected",
                releaseRequired=release_required, rustFull=rust_full,
                rustPackages=sorted(set(rust_packages)), jobs=jobs, openObligations=sorted(obligations))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for flag in ("paths", "metadata", "source-revision", "event", "base-ref", "head-ref", "out"):
        parser.add_argument("--" + flag, required=True)
    for flag in ("full", "release-required", "diff-incomplete"):
        parser.add_argument("--" + flag, action="store_true")
    args = parser.parse_args()
    try:
        try:
            paths = affected.read_paths(args.paths)
        except OSError:
            if not args.diff_incomplete:
                raise
            paths = []
        meta = affected.load_metadata(args.metadata)
        result = make_plan(meta, paths, detect_inventory(Path(meta["workspace_root"]), paths),
                           source_revision=args.source_revision, event_name=args.event, base_ref=args.base_ref,
                           head_ref=args.head_ref, full=args.full, release_required=args.release_required,
                           diff_complete=not args.diff_incomplete)
        Path(args.out).write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        if any(job["decision"] == "blocked" for job in result["jobs"].values()):
            parser.exit(1, "plan: blocked applicability; see output missingInputs\n")
    except (OSError, ValueError, KeyError, AssertionError) as error:
        parser.exit(2, f"plan: {error}\n")


if __name__ == "__main__":
    main()
