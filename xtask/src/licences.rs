//! `cargo xtask licences [--check]`: THIRD-PARTY-LICENSES.txt, the licences of every crate built
//! into the shipped programs (solidcraft, solidcraft-cli, solidcraft-web), read from
//! `cargo metadata` and the crates' own licence files. The Rust programs embed this Cargo
//! inventory (Help ▸ About ▸ Licences, `solidcraft-cli licences`), plus bundled font notices.
//! This is not an npm/Node licence or per-file source-rights gate. The desktop Mastra
//! companion needs separate delivered licences/notices and exact artifact admission;
//! see `docs/mastra-distribution.md`. Cargo success does not approve runtime bytes or rights.
//! `--check` (part of `cargo xtask ci`) generates it without writing it: it fails when the
//! dependency graph can't be read, and only reports a file that is out of date, since pull
//! requests leave it to the Generated files workflow (`.github/workflows/generated.yml`), which
//! rewrites it on main after each merge.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

use serde_json::Value;

use crate::cargo;

pub const FILE: &str = "THIRD-PARTY-LICENSES.txt";
const ROOTS: &[&str] = &["solidcraft", "solidcraft-cli", "solidcraft-web"];

pub fn run(root: &Path, check: bool) -> Result<(), String> {
    let text = generate(root)?;
    let path = root.join(FILE);
    if check {
        let old = std::fs::read_to_string(&path).unwrap_or_default();
        if old.replace("\r\n", "\n") != text {
            // Not an error: pull requests don't edit the file (two that did would always conflict);
            // the Generated files workflow rewrites it on main after the merge.
            println!("licences: {FILE} will change when this lands (the Generated files workflow rewrites it on main)");
            return Ok(());
        }
        println!("licences: {FILE} is up to date");
        return Ok(());
    }
    std::fs::write(&path, &text).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("licences: wrote {FILE} ({} KB)", text.len() / 1024);
    Ok(())
}

struct Crate {
    name: String,
    version: String,
    license: String,
    repository: String,
    /// (file name, text) of the licence files shipped in the crate.
    files: Vec<(String, String)>,
}

fn generate(root: &Path) -> Result<String, String> {
    let out = cargo().args(["metadata", "--format-version", "1", "--locked"]).output().map_err(|e| format!("cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(format!("cargo metadata failed:\n{}", String::from_utf8_lossy(&out.stderr)));
    }
    let meta: Value = serde_json::from_slice(&out.stdout).map_err(|e| format!("cargo metadata JSON: {e}"))?;
    let packages: BTreeMap<&str, &Value> = meta["packages"].as_array().into_iter().flatten().filter_map(|p| Some((p["id"].as_str()?, p))).collect();
    let nodes: BTreeMap<&str, &Value> =
        meta["resolve"]["nodes"].as_array().into_iter().flatten().filter_map(|n| Some((n["id"].as_str()?, n))).collect();
    let members: BTreeSet<&str> = meta["workspace_members"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();

    // Everything reachable from the shipped programs through normal (not dev) dependencies, on any
    // platform.
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut queue: VecDeque<&str> = packages
        .iter()
        .filter(|(id, p)| members.contains(*id) && ROOTS.contains(&p["name"].as_str().unwrap_or_default()))
        .map(|(id, _)| *id)
        .collect();
    while let Some(id) = queue.pop_front() {
        if !seen.insert(id) {
            continue;
        }
        for d in nodes.get(id).and_then(|n| n["deps"].as_array()).into_iter().flatten() {
            let normal = d["dep_kinds"].as_array().into_iter().flatten().any(|k| k["kind"].is_null());
            if normal && let Some(pkg) = d["pkg"].as_str() {
                queue.push_back(pkg);
            }
        }
    }

    let mut crates = Vec::new();
    for id in seen {
        let Some(p) = packages.get(id) else { continue };
        if members.contains(id) {
            continue;
        }
        let s = |k: &str| p[k].as_str().unwrap_or_default().to_string();
        let dir = Path::new(p["manifest_path"].as_str().unwrap_or_default()).parent().map(Path::to_path_buf).unwrap_or_default();
        crates.push(Crate {
            name: s("name"),
            version: s("version"),
            license: s("license"),
            repository: s("repository"),
            files: licence_files(&dir, p),
        });
    }
    crates.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
    Ok(render(root, &crates))
}

/// The licence and notice files at the top of a crate's package (and its `license-file`).
fn licence_files(dir: &Path, p: &Value) -> Vec<(String, String)> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            let u = n.to_ascii_uppercase();
            if e.path().is_file() && ["LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE", "COPYRIGHT"].iter().any(|k| u.starts_with(k)) {
                names.insert(n);
            }
        }
    }
    if let Some(f) = p["license_file"].as_str() {
        names.insert(f.to_string());
    }
    names
        .into_iter()
        .filter_map(|n| {
            let text = std::fs::read_to_string(dir.join(&n)).ok()?;
            let text = text.replace("\r\n", "\n").trim().to_string();
            (!text.is_empty()).then(|| (Path::new(&n).file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or(n), text))
        })
        .collect()
}

fn render(root: &Path, crates: &[Crate]) -> String {
    let rule = "=".repeat(80);
    let mut out = format!(
        "Third-party software in SolidCraft\n\nSolidCraft (the desktop app, solidcraft-cli and the web app) is built from the {} crates below.\n\
         Generated by `cargo xtask licences` from Cargo.lock and the crates' own licence files.\n\
         SolidCraft's own licence is in LICENSE-MIT and LICENSE-APACHE, and NOTICE.\n\n",
        crates.len()
    );
    for c in crates {
        out += &format!("{} {}  ({})\n", c.name, c.version, if c.license.is_empty() { "see its licence file" } else { &c.license });
    }
    // One copy of each distinct licence text, with the crates it covers.
    let mut by_text: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut without: Vec<&Crate> = Vec::new();
    for c in crates {
        if c.files.is_empty() {
            without.push(c);
        }
        for (_, text) in &c.files {
            by_text.entry(text.as_str()).or_default().push(format!("{} {}", c.name, c.version));
        }
    }
    let mut sections: Vec<(Vec<String>, &str)> = by_text.into_iter().map(|(t, v)| (v, t)).collect();
    sections.sort();
    for (names, text) in sections {
        out += &format!("\n{rule}\n{}\n{rule}\n\n{text}\n", wrap(&names.join(", ")));
    }
    if !without.is_empty() {
        out += &format!(
            "\n{rule}\nCrates whose package carries no licence file (their licence is named in Cargo.toml; the MIT and Apache-2.0\ntexts are as in SolidCraft's own LICENSE-MIT and LICENSE-APACHE)\n{rule}\n\n"
        );
        for c in without {
            out += &format!(
                "{} {}: {}{}\n",
                c.name,
                c.version,
                c.license,
                if c.repository.is_empty() { String::new() } else { format!(" ({})", c.repository) }
            );
        }
    }

    // Bundled font licences (SIL Open Font License, Version 1.1)
    let font_rule = "=".repeat(80);
    out += &format!(
        "\n{font_rule}\nBundled fonts (SIL Open Font License, Version 1.1)\n{font_rule}\n\n\
         - LiberationSans-Regular.ttf (assets/fonts/liberation-sans/LiberationSans-Regular.ttf)\n  \
           Copyright 2012 Red Hat, Inc. (Liberation Fonts 2.x; digitized data Google)\n  \
           Source: https://github.com/liberationfonts\n\n\
         - BIZUDPGothic-Regular.ttf (assets/fonts/biz-udp-gothic/BIZUDPGothic-Regular.ttf)\n  \
           Copyright 2022 The BIZ UDGothic Project Authors (https://github.com/googlefonts/morisawa-biz-ud-mincho)\n  \
           Source: https://github.com/googlefonts/morisawa-biz-ud-gothic\n\n\
         - CraftUI-SC.otf (assets/fonts/craft-ui-sc/CraftUI-SC.otf)\n  \
           Copyright 2014-2021 Adobe / Google Inc. (Noto Sans SC subset for SolidCraft web zh)\n  \
           Source: https://github.com/notofonts/noto-cjk (Sans SubsetOTF/SC)\n\n\
         - CraftUI-TC.otf (assets/fonts/craft-ui-tc/CraftUI-TC.otf)\n  \
           Copyright 2014-2021 Adobe / Google Inc. (Noto Sans TC subset for SolidCraft web zh-TW)\n  \
           Source: https://github.com/notofonts/noto-cjk (Sans SubsetOTF/TC)\n\n\
         - IBMPlexSans-Variable.ttf (assets/fonts/ibm-plex-sans/IBMPlexSans-Variable.ttf)\n  \
           Copyright 2017 IBM Corp. with Reserved Font Name \"Plex\"\n  \
           Source: https://github.com/IBM/plex\n\n\
         - IBMPlexMono-Regular.ttf (assets/fonts/ibm-plex-mono/IBMPlexMono-Regular.ttf)\n  \
           Copyright 2017 IBM Corp. with Reserved Font Name \"Plex\"\n  \
           Source: https://github.com/IBM/plex\n\n"
    );
    let ofl_path = root.join("assets/fonts/biz-udp-gothic/OFL.txt");
    let fallback_path = root.join("assets/fonts/liberation-sans/OFL.txt");
    let ofl_text = std::fs::read_to_string(&ofl_path).or_else(|_| std::fs::read_to_string(&fallback_path)).unwrap_or_default();
    let ofl_clean = ofl_text.replace("\r\n", "\n").trim().to_string();
    if !ofl_clean.is_empty() {
        out += &format!("{font_rule}\nSIL OPEN FONT LICENSE Version 1.1\n{font_rule}\n\n{ofl_clean}\n");
    }

    out
}

/// Wrap a long list of crate names at 100 columns.
fn wrap(s: &str) -> String {
    let mut out = String::new();
    let mut col = 0;
    for w in s.split(' ') {
        if col > 0 && col + w.len() + 1 > 100 {
            out.push('\n');
            col = 0;
        } else if col > 0 {
            out.push(' ');
            col += 1;
        }
        out += w;
        col += w.len();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_artifact::{
        ErrorCode, GateError, GateStatus, Inventory, Metadata, Mode, ObservedFile, Policy, Request, RuntimeReceipt, SourceRecords, TreeKind,
        check_repository_policy, evaluate, inspect_tree, parse_args, parse_metadata, read_metadata, read_policy, read_source_records, test_support,
        write_result,
    };
    use serde_json::json;
    use sha2::{Digest as _, Sha256};
    use std::path::PathBuf;

    // Mechanical policy anchors supplement, not replace, fresh-context guidance scenarios.
    // Losing the scoped exception or its audit caveats must not silently restore a blanket rule.
    #[test]
    fn distribution_policy_guidance_is_narrow_and_router_bounded() {
        let root = crate::root();
        let read = |path: &str| std::fs::read_to_string(root.join(path)).unwrap().replace("\r\n", "\n");
        for path in ["AGENTS.md", "CONTRIBUTING.md", "skills/solidcraft-ground-rules/SKILL.md", "skills/solidcraft-packaging/SKILL.md"] {
            assert!(read(path).contains("desktop-only Mastra distribution exception"), "{path}: missing shared policy marker");
        }
        let ground_rules = read("skills/solidcraft-ground-rules/SKILL.md");
        for phrase in
            ["docs/mastra-distribution.md", "packaging/mastra/source-records.json", "per-file", "unverified", "third-party", "public redistribution"]
        {
            assert!(ground_rules.contains(phrase), "ground-rules: missing {phrase}");
        }
        for path in ["CONTRIBUTING.md", "skills/solidcraft-packaging/SKILL.md", "docs/mastra-distribution.md"] {
            let text = read(path);
            for phrase in ["Cargo", "npm/Node", "#7", "#14"] {
                assert!(text.contains(phrase), "{path}: missing coverage/deferred-acceptance anchor {phrase}");
            }
        }
        // Consumer handoff anchors, not another schema: Rust DTOs remain executable authority.
        let distribution = read("docs/mastra-distribution.md");
        for dto in [
            "Policy",
            "Approval",
            "SourceRecords",
            "Authorization",
            "SourceRecord",
            "Review",
            "FileRef",
            "Inventory",
            "Node",
            "PackageManager",
            "Dependency",
            "LicenseEvidence",
            "BuildInput",
            "Output",
            "Derivation",
            "BuildEvidence",
            "RuntimeReceipt",
        ] {
            assert!(distribution.contains(&format!("{dto} {{")), "distribution contract: missing {dto} shape");
        }
        for phrase in [
            "All fields are required",
            "Missing is not null",
            "unknown and duplicate",
            "packageManager: PackageManager",
            "rootDependencyIds: Id[]",
            "rootToolIds: Id[]",
            "lockIntegrity: Text",
            "CI-owned, quiescent",
            "not adversarial filesystem isolation",
        ] {
            assert!(distribution.contains(phrase), "distribution contract: missing {phrase}");
        }
    }

    struct DistributionFixture {
        root: PathBuf,
        repository: PathBuf,
        artifact: PathBuf,
        inputs: PathBuf,
    }

    impl Drop for DistributionFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn distribution_fixture() -> DistributionFixture {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = loop {
            let candidate = std::env::temp_dir().join(format!("xtask-distribution-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            match std::fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create fixture: {e}"),
            }
        };
        // Owned test paths must not inherit platform temp-directory aliases (e.g. /var on macOS).
        let root = std::fs::canonicalize(root).unwrap();
        let fixture = DistributionFixture { repository: root.join("repository"), artifact: root.join("artifact"), inputs: root.join("inputs"), root };
        for directory in [&fixture.repository, &fixture.artifact, &fixture.inputs] {
            std::fs::create_dir(directory).unwrap();
        }
        fixture
    }

    fn put_bytes(root: &Path, path: &str, bytes: &[u8]) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn digest(bytes: &[u8]) -> String {
        format!("sha256:{:x}", Sha256::digest(bytes))
    }

    fn rewrite_json(root: &Path, path: &str, edit: impl FnOnce(&mut Value)) {
        let mut value = serde_json::from_slice(&std::fs::read(root.join(path)).unwrap()).unwrap();
        edit(&mut value);
        put_bytes(root, path, &serde_json::to_vec(&value).unwrap());
    }

    fn reapprove(fixture: &DistributionFixture) {
        let inventory_digest = digest(&std::fs::read(fixture.artifact.join("inventory.json")).unwrap());
        rewrite_json(&fixture.repository, "packaging/mastra/policy.json", |p| {
            p["approvals"] = json!([{"id":"fixture-approval", "target":"x86_64-unknown-linux-gnu", "inventoryDigest":inventory_digest}]);
        });
        let policy_digest = digest(&std::fs::read(fixture.repository.join("packaging/mastra/policy.json")).unwrap());
        rewrite_json(&fixture.artifact, "runtime.json", |r| {
            r["provenanceInventory"]["digest"] = json!(inventory_digest);
            r["policyDigest"] = json!(policy_digest);
        });
    }

    fn valid_artifact_case() -> DistributionFixture {
        let fixture = distribution_fixture();
        let revision = "1234567890abcdef1234567890abcdef12345678";
        let target = "x86_64-unknown-linux-gnu";
        let pin = digest(b"original synthetic archive, not an upstream package");
        let sources = json!({"schemaVersion":"1.0", "authorizations":[{
            "id":"fixture-grant", "actor":"Synthetic owner", "declaration":"Synthetic authorization",
            "proposition":"Original test material only", "sourceRepository":"https://fixture.invalid/source", "sourceRevision":revision,
            "eligibleRoles":["agent-loop","model-runtime","orchestration"],
            "excludedRoles":["account","database","script-execution","server-bootstrap"],
            "licenseGrant":"MIT OR Apache-2.0", "evidence":"Original synthetic test grant"
        }], "records":[{"id":"fixture-source", "authorizationId":"fixture-grant", "role":"orchestration",
            "sourceRepository":"https://fixture.invalid/source", "sourceRevision":revision, "sourcePath":"src/original.ts",
            "sourceDigest":null, "destinationPath":null, "destinationDigest":null, "adaptation":null,
            "rightsHolder":null, "licenseExpression":null, "requiredNotices":[], "dependencyIds":[],
            "thirdPartyReview":{"status":"unverified","evidence":[]}, "verification":{"status":"unverified","evidence":[]}}]});
        let policy = json!({"schemaVersion":"1.0", "policyId":"fixture-policy", "sourceRecordsPath":"packaging/mastra/source-records.json",
            "supportedTargets":["aarch64-apple-darwin","x86_64-apple-darwin","x86_64-pc-windows-msvc",target],
            "allowedLicenseExpressions":["Apache-2.0","MIT","MIT OR Apache-2.0"], "forbiddenPathComponents":["ee"], "approvals":[]});
        put_bytes(&fixture.repository, "packaging/mastra/policy.json", &serde_json::to_vec(&policy).unwrap());
        put_bytes(&fixture.repository, "packaging/mastra/source-records.json", &serde_json::to_vec(&sources).unwrap());
        let mut inputs = Vec::new();
        let mut outputs = Vec::new();
        let mut derivations = Vec::new();
        for (id, path, bytes, origin, origin_id, role) in [
            ("code-input", "dist/main.js", b"export const fixture = 1;\n".as_slice(), "dependency", "fixture-dependency", "code"),
            (
                "license-input",
                "licenses/original.txt",
                b"Original test permission and notice.\n".as_slice(),
                "repository",
                "license-input",
                "license",
            ),
            ("node-input", "node", b"Original inert Node stand-in; never execute.\n".as_slice(), "node", "node", "node"),
        ] {
            put_bytes(&fixture.inputs, path, bytes);
            put_bytes(&fixture.artifact, path, bytes);
            if origin == "repository" {
                put_bytes(&fixture.repository, path, bytes);
            }
            inputs.push(json!({"id":id,"materializedPath":path,"originKind":origin,"originId":origin_id,"originalPath":path,
                "sourceRevision":null, "digest":digest(bytes),
                "sizeBytes":bytes.len(),"licenseIds":["fixture-license"],"sourceRecordId":null}));
            outputs.push(json!({"path":path,"digest":digest(bytes),"sizeBytes":bytes.len(),"role":role,"licenseIds":["fixture-license"]}));
            derivations.push(json!({"outputPath":path,"inputIds":[id],"toolInputIds":[],"evidenceInputId":null}));
        }
        let file_ref = |path: &str, bytes: &[u8]| {
            put_bytes(&fixture.repository, path, bytes);
            json!({"path":path,"digest":digest(bytes)})
        };
        let inventory = json!({"schemaVersion":"1.0","artifactId":"fixture-artifact","target":target,"runtimeVersion":"1.2.3-rc.1",
            "protocolVersions":["1.0"],"node":{"version":"22.1.0","sourceUrl":"https://fixture.invalid/node","archiveDigest":pin,"executablePath":"node"},
            "packageManager":{"name":"npm","version":"10.1.0","sourceUrl":"https://fixture.invalid/npm","archiveDigest":pin},
            "lockfile":file_ref("fixture.lock", b"original synthetic lock"),"contractManifest":file_ref("contract.json",b"original synthetic contract"),
            "rootDependencyIds":["fixture-dependency"],"dependencies":[{"id":"fixture-dependency","name":"@fixture/original","version":"1.0.0",
                "sourceUrl":"https://fixture.invalid/dependency","archiveDigest":pin,"lockIntegrity":"synthetic-integrity","dependencyIds":[]}],
            "rootToolIds":[],"buildTools":[],"licenses":[{"id":"fixture-license","expression":"MIT","rightsHolder":"Synthetic fixture author",
                "licenseFiles":[{"path":"licenses/original.txt","digest":outputs[1]["digest"]}],"noticeFiles":[]}],
            "inputs":inputs,"outputs":outputs,"derivations":derivations});
        put_bytes(&fixture.artifact, "inventory.json", &serde_json::to_vec(&inventory).unwrap());
        let receipt = json!({"schemaVersion":"1.0","runtimeVersion":"1.2.3-rc.1","protocolVersions":["1.0"],"targetTriple":target,
            "nodeVersion":"22.1.0","repositoryRevision":revision,"contractDigest":inventory["contractManifest"]["digest"],"policyDigest":pin,
            "sourceRecordsDigest":digest(&serde_json::to_vec(&sources).unwrap()),"provenanceInventory":{"path":"inventory.json","digest":pin},
            "artifactDigests":outputs.iter().map(|o| json!({"path":o["path"],"digest":o["digest"]})).collect::<Vec<_>>()});
        put_bytes(&fixture.artifact, "runtime.json", &serde_json::to_vec(&receipt).unwrap());
        reapprove(&fixture);
        fixture
    }

    fn fixture_json(root: &Path, path: &str) -> Value {
        serde_json::from_slice(&std::fs::read(root.join(path)).unwrap()).unwrap()
    }

    // Trusted expectations come from owned repository bytes, NOT observed receipt/inventory text.
    // This synthetic current revision is deliberately different from the upstream in migration tests.
    fn artifact_request(f: &DistributionFixture) -> Request {
        Request {
            mode: Mode::Artifact,
            tree: Some(f.artifact.clone()),
            tree_kind: None,
            inputs_root: Some(f.inputs.clone()),
            approval_id: Some("fixture-approval".into()),
            target: Some("x86_64-unknown-linux-gnu".into()),
            expected_revision: Some("1234567890abcdef1234567890abcdef12345678".into()),
            expected_contract_digest: Some(digest(&std::fs::read(f.repository.join("contract.json")).unwrap())),
            expected_policy_digest: Some(digest(&std::fs::read(f.repository.join("packaging/mastra/policy.json")).unwrap())),
            expected_sources_digest: Some(digest(&std::fs::read(f.repository.join("packaging/mastra/source-records.json")).unwrap())),
            advertised: Some(true),
        }
    }

    fn artifact_error(f: &DistributionFixture, request: &Request, want: &str) {
        let err = evaluate(&f.repository, request).unwrap_err();
        assert_eq!(serde_json::to_value(&err.code).unwrap(), want, "{err:?}");
        assert!(!err.subject.as_deref().is_some_and(|s| s.contains(f.root.to_str().unwrap())));
    }

    fn refresh_fixture_receipt(f: &DistributionFixture) {
        let inventory = fixture_json(&f.artifact, "inventory.json");
        let sources = digest(&std::fs::read(f.repository.join("packaging/mastra/source-records.json")).unwrap());
        rewrite_json(&f.artifact, "runtime.json", |r| {
            r["sourceRecordsDigest"] = json!(sources);
            r["artifactDigests"] = json!(
                inventory["outputs"].as_array().unwrap().iter().map(|o| { json!({"path":o["path"],"digest":o["digest"]}) }).collect::<Vec<_>>()
            );
        });
        // Explicit synthetic reapproval, never called by the verifier or a real producer.
        reapprove(f);
    }

    // Build and execute the production entrypoint, not the test harness or a parser surrogate.
    fn distribution_binary() -> &'static Path {
        static BINARY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        BINARY.get_or_init(|| {
            let executable = std::env::current_exe().unwrap();
            let profile = executable.parent().unwrap().parent().unwrap();
            // Outer --target-dir is not inherited by nested Cargo. Keep this build isolated
            // beside the test profile, but execute Cargo's returned ci binary even in dev tests.
            let target_dir = profile.parent().unwrap();
            let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
            let output = crate::cargo()
                .args(["build", "--profile", "ci", "--locked", "-p", "xtask", "--bin", "xtask", "--message-format=json-render-diagnostics"])
                .arg("--manifest-path")
                .arg(&manifest)
                .arg("--target-dir")
                .arg(target_dir)
                .output()
                .unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            let stdout = String::from_utf8(output.stdout).unwrap();
            let mut artifacts = stdout.lines().map(|line| serde_json::from_str::<Value>(line).unwrap()).filter(|artifact| {
                artifact["reason"] == "compiler-artifact"
                    && artifact["target"]["name"] == "xtask"
                    && artifact["target"]["kind"] == json!(["bin"])
                    && artifact["manifest_path"].as_str().map(Path::new) == Some(manifest.as_path())
            });
            let artifact = artifacts.next().expect("successful build must return the current xtask bin");
            assert!(artifacts.next().is_none(), "ambiguous xtask executable");
            eprintln!("distribution CLI build: target-dir={} profile=ci artifact={artifact}", target_dir.display());
            PathBuf::from(artifact["executable"].as_str().expect("xtask compiler artifact must name its executable"))
        })
    }

    fn distribution_cli(args: &[&str], exit: i32) -> Value {
        let output = std::process::Command::new(distribution_binary()).arg("runtime-artifact").args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(exit), "stderr: {}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(
            output.stdout.iter().filter(|b| **b == b'\n').count(),
            1,
            "stdout: {:?}; stderr: {}",
            output.stdout,
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout.last(), Some(&b'\n'));
        if exit == 0 {
            assert!(output.stderr.is_empty(), "{:?}", output.stderr);
        } else {
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(String::from_utf8(output.stderr).unwrap(), format!("error: {}\n", value["error"]["code"].as_str().unwrap()));
        }
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn artifact_cli_args(f: &DistributionFixture) -> Vec<String> {
        let request = artifact_request(f);
        vec![
            "--mode".into(),
            "artifact".into(),
            "--tree".into(),
            f.artifact.to_str().unwrap().into(),
            "--inputs-root".into(),
            f.inputs.to_str().unwrap().into(),
            "--approval".into(),
            "fixture-approval".into(),
            "--target".into(),
            "x86_64-unknown-linux-gnu".into(),
            "--expected-revision".into(),
            request.expected_revision.unwrap(),
            "--expected-contract-digest".into(),
            request.expected_contract_digest.unwrap(),
            "--expected-policy-digest".into(),
            request.expected_policy_digest.unwrap(),
            "--expected-sources-digest".into(),
            request.expected_sources_digest.unwrap(),
            "--advertised".into(),
            "true".into(),
        ]
    }

    fn string_refs(args: &[String]) -> Vec<&str> {
        args.iter().map(String::as_str).collect()
    }

    // Missing dispatch, implicit success/fallback, or main ignoring the gate's Err breaks this.
    #[test]
    fn distribution_cli_never_downgrades_artifact_requests() {
        let policy = distribution_cli(&["--mode", "policy"], 0);
        eprintln!("distribution CLI executed: {}", distribution_binary().display());
        assert_eq!((policy["mode"].as_str(), policy["status"].as_str()), (Some("policy"), Some("policy_valid")));
        for args in [
            vec![],
            vec!["--mode", "artifact"],
            vec!["--mode", "host-only"],
            vec!["--mode", "policy", "--tree", "."],
            vec!["--mode", "policy", "--policy", "/private/policy.json"],
            vec!["--mode", "policy", "--mode", "policy"],
            vec!["--mode", "nonsense"],
        ] {
            let result = distribution_cli(&args, 1);
            assert_eq!(result["status"], "failed");
            assert_eq!(result["error"]["code"], "INVALID_ARGUMENT");
        }
        let f = valid_artifact_case();
        let artifact = artifact_cli_args(&f);
        let host = [
            "--mode",
            "host-only",
            "--tree",
            ".",
            "--tree-kind",
            "repository",
            "--target",
            "x86_64-unknown-linux-gnu",
            "--expected-revision",
            "1234567890abcdef1234567890abcdef12345678",
            "--advertised",
            "false",
        ]
        .map(str::to_owned)
        .to_vec();
        for (args, mode, status) in [(&artifact, Mode::Artifact, "verified"), (&host, Mode::HostOnly, "not_applicable")] {
            let request = parse_args(&string_refs(args)).unwrap();
            assert_eq!(request.mode, mode);
            let report = evaluate(&f.repository, &request).unwrap();
            assert_eq!(serde_json::to_value(report.status).unwrap(), status);
            // Every required flag is required; each flag may occur only once, even with the same value.
            for index in (0..args.len()).step_by(2) {
                let mut missing = args.clone();
                missing.drain(index..index + 2);
                assert_eq!(parse_args(&string_refs(&missing)).unwrap_err().code, ErrorCode::InvalidArgument, "{missing:?}");
                let mut repeated = args.clone();
                repeated.extend_from_slice(&args[index..index + 2]);
                assert_eq!(parse_args(&string_refs(&repeated)).unwrap_err().code, ErrorCode::InvalidArgument, "{repeated:?}");
            }
            for extra in [
                vec!["--unknown", "value"],
                vec!["--policy", "/private/review.json"],
                vec!["--sources", "other.json"],
                vec!["extra"],
                vec!["--approval"],
                vec!["--advertised=true"],
            ] {
                let mut bad = args.clone();
                bad.extend(extra.iter().map(|s| (*s).to_owned()));
                assert_eq!(parse_args(&string_refs(&bad)).unwrap_err().code, ErrorCode::InvalidArgument);
            }
        }
        for (original, flag, value) in [
            (&host, "--advertised", "true"),
            (&host, "--tree-kind", "package"),
            (&artifact, "--advertised", "false"),
            (&artifact, "--advertised", "TRUE"),
            (&artifact, "--approval", ""),
            (&artifact, "--approval", "../unreviewed"),
            (&artifact, "--target", ""),
            (&artifact, "--expected-revision", "HEAD"),
            (&artifact, "--expected-contract-digest", "sha256:missing"),
            (&artifact, "--expected-policy-digest", ""),
            (&artifact, "--expected-sources-digest", "sha256:BAD"),
            (&artifact, "--tree", ""),
            (&artifact, "--inputs-root", ""),
        ] {
            let mut bad = original.clone();
            let index = bad.iter().position(|arg| arg == flag).unwrap();
            bad[index + 1] = value.into();
            assert_eq!(parse_args(&string_refs(&bad)).unwrap_err().code, ErrorCode::InvalidArgument, "{flag} {value}");
            assert_eq!(distribution_cli(&string_refs(&bad), 1)["error"]["code"], "INVALID_ARGUMENT");
        }
        for (original, extra) in [
            (&artifact, vec!["--tree-kind", "package"]),
            (&host, vec!["--inputs-root", "."]),
            (&host, vec!["--approval", "fixture-approval"]),
            (&host, vec!["--expected-contract-digest", "ignored"]),
            (&host, vec!["--expected-policy-digest", "ignored"]),
            (&host, vec!["--expected-sources-digest", "ignored"]),
        ] {
            let mut bad = original.clone();
            bad.extend(extra.iter().map(|s| (*s).to_owned()));
            assert_eq!(parse_args(&string_refs(&bad)).unwrap_err().code, ErrorCode::InvalidArgument);
        }
        for flag in [
            "--tree",
            "--tree-kind",
            "--inputs-root",
            "--approval",
            "--target",
            "--expected-revision",
            "--expected-contract-digest",
            "--expected-policy-digest",
            "--expected-sources-digest",
            "--advertised",
        ] {
            assert_eq!(parse_args(&["--mode", "policy", flag, "value"]).unwrap_err().code, ErrorCode::InvalidArgument);
        }
        // These complete requests still cannot become host-only, even on no-runtime targets.
        for target in ["wasm32-unknown-unknown", "aarch64-unknown-linux-gnu", "x86_64-unknown-freebsd", "unsupported"] {
            let mut bad = artifact.clone();
            let index = bad.iter().position(|arg| arg == "--target").unwrap();
            bad[index + 1] = target.into();
            let request = parse_args(&string_refs(&bad)).unwrap();
            assert_eq!(evaluate(&f.repository, &request).unwrap_err().code, ErrorCode::UnsupportedTarget);
            let result = distribution_cli(&string_refs(&bad), 1);
            assert_eq!(result["mode"], "artifact");
            assert_eq!(result["error"]["code"], "UNSUPPORTED_TARGET");
        }
        for flag in ["--tree", "--inputs-root"] {
            let mut missing = artifact.clone();
            let index = missing.iter().position(|arg| arg == flag).unwrap();
            missing[index + 1] = f.root.join("deliberately-absent").to_str().unwrap().into();
            let request = parse_args(&string_refs(&missing)).unwrap();
            assert_eq!(evaluate(&f.repository, &request).unwrap_err().code, ErrorCode::IoError);
            let result = distribution_cli(&string_refs(&missing), 1);
            assert_eq!(result["mode"], "artifact");
            assert_eq!(result["status"], "failed");
            assert_eq!(result["error"], json!({"code":"IO_ERROR", "subject":null}));
        }
        let mut raw = artifact_request(&f);
        for field in ["tree", "inputs", "approval", "revision", "contract", "policy", "sources", "target"] {
            let mut incomplete = raw.clone();
            match field {
                "tree" => incomplete.tree = None,
                "inputs" => incomplete.inputs_root = None,
                "approval" => incomplete.approval_id = None,
                "revision" => incomplete.expected_revision = None,
                "contract" => incomplete.expected_contract_digest = None,
                "policy" => incomplete.expected_policy_digest = None,
                "sources" => incomplete.expected_sources_digest = None,
                "target" => incomplete.target = None,
                _ => unreachable!(),
            }
            assert_eq!(evaluate(&f.repository, &incomplete).unwrap_err().code, ErrorCode::InvalidArgument);
        }
        raw.tree_kind = Some(TreeKind::Package);
        assert_eq!(evaluate(&f.repository, &raw).unwrap_err().code, ErrorCode::InvalidArgument);
        // Reviewed metadata wins over requested-mode requirements for direct evaluate callers.
        put_bytes(&f.repository, "packaging/mastra/policy.json", b"{\"secret\":\"never disclose JSON content\"}");
        assert_eq!(evaluate(&f.repository, &raw).unwrap_err().code, ErrorCode::InvalidMetadata);
    }

    // A failed artifact must keep its mode and exact failure shape, never a foundation pass.
    #[test]
    fn distribution_report_distinguishes_foundation_from_artifact_evidence() {
        let result = distribution_cli(&["--mode", "artifact"], 1);
        eprintln!("distribution CLI executed: {}", distribution_binary().display());
        assert_eq!(
            result,
            json!({"schemaVersion":"1.0","mode":"artifact","status":"failed",
            "error":{"code":"INVALID_ARGUMENT","subject":null}})
        );
        for (args, mode) in [
            (vec![], Value::Null),
            (vec!["--mode", "bad"], Value::Null),
            (vec!["--mode", "artifact", "--mode", "artifact"], Value::Null),
            (vec!["--mode", "host-only", "--unknown", "x"], json!("host_only")),
            (vec!["--mode", "policy", "--unknown", "x"], json!("policy")),
        ] {
            assert_eq!(
                distribution_cli(&args, 1),
                json!({"schemaVersion":"1.0","mode":mode,"status":"failed",
                "error":{"code":"INVALID_ARGUMENT","subject":null}})
            );
        }
        let f = valid_artifact_case();
        let policy_digest = digest(&std::fs::read(f.repository.join("packaging/mastra/policy.json")).unwrap());
        let sources_digest = digest(&std::fs::read(f.repository.join("packaging/mastra/source-records.json")).unwrap());
        let inventory_digest = digest(&std::fs::read(f.artifact.join("inventory.json")).unwrap());
        let host = [
            "--mode",
            "host-only",
            "--tree",
            ".",
            "--tree-kind",
            "repository",
            "--target",
            "x86_64-unknown-linux-gnu",
            "--expected-revision",
            "1234567890abcdef1234567890abcdef12345678",
            "--advertised",
            "false",
        ];
        for (args, expected) in [
            (
                vec!["--mode".into(), "policy".into()],
                json!({"schemaVersion":"1.0","mode":"policy","status":"policy_valid",
                "target":null,"treeKind":null,"policyDigest":policy_digest,"sourceRecordsDigest":sources_digest,"inventoryDigest":null,
                "filesChecked":0,"inputsChecked":0,"dependenciesChecked":0}),
            ),
            (
                host.map(str::to_owned).to_vec(),
                json!({"schemaVersion":"1.0","mode":"host_only","status":"not_applicable",
                "target":"x86_64-unknown-linux-gnu","treeKind":"repository","policyDigest":policy_digest,"sourceRecordsDigest":sources_digest,
                "inventoryDigest":null,"filesChecked":5,"inputsChecked":0,"dependenciesChecked":0}),
            ),
            (
                artifact_cli_args(&f),
                json!({"schemaVersion":"1.0","mode":"artifact","status":"verified",
                "target":"x86_64-unknown-linux-gnu","treeKind":"package","policyDigest":policy_digest,"sourceRecordsDigest":sources_digest,
                "inventoryDigest":inventory_digest,"filesChecked":5,"inputsChecked":3,"dependenciesChecked":1}),
            ),
        ] {
            let request = parse_args(&string_refs(&args)).unwrap();
            let report = evaluate(&f.repository, &request);
            let mut bytes = Vec::new();
            write_result(&mut bytes, Some(request.mode), &report).unwrap();
            assert_eq!(bytes.iter().filter(|b| **b == b'\n').count(), 1);
            assert_eq!(bytes.last(), Some(&b'\n'));
            assert!(!bytes.windows(2).any(|w| w == b": " || w == b", "));
            assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), expected);
        }
        // No raw source, absolute filesystem location or parser snippet may cross the report boundary.
        for (subject, expected) in [
            ("/private/source.json", None),
            ("C:\\private\\source.json", None),
            ("../secret", None),
            ("{\"secret\":\"raw source\"}", None),
            ("line\nsecret", None),
            ("dist/main.js", Some("dist/main.js")),
            ("fixture-approval", Some("fixture-approval")),
            ("src/EE/input.ts", Some("src/EE/input.ts")),
        ] {
            let failure = Err(GateError { code: ErrorCode::InvalidMetadata, subject: Some(subject.into()) });
            let mut bytes = Vec::new();
            write_result(&mut bytes, Some(Mode::Artifact), &failure).unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&bytes).unwrap(),
                json!({"schemaVersion":"1.0","mode":"artifact","status":"failed",
                "error":{"code":"INVALID_METADATA","subject":expected}})
            );
        }
        struct BrokenWriter {
            fail_flush: bool,
        }
        impl std::io::Write for BrokenWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.fail_flush { Ok(bytes.len()) } else { Err(std::io::ErrorKind::BrokenPipe.into()) }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }
        let success = evaluate(&f.repository, &parse_args(&["--mode", "policy"]).unwrap());
        for fail_flush in [false, true] {
            assert_eq!(write_result(&mut BrokenWriter { fail_flush }, Some(Mode::Policy), &success).unwrap_err().code, ErrorCode::IoError);
            let failure = Err(GateError { code: ErrorCode::InvalidArgument, subject: None });
            assert_eq!(write_result(&mut BrokenWriter { fail_flush }, None, &failure).unwrap_err().code, ErrorCode::IoError);
        }
        // Strict parser errors point only at the fixed metadata path, never its bytes.
        put_bytes(&f.repository, "packaging/mastra/source-records.json", b"{\"secret\":\"do not disclose\"}");
        let failure = evaluate(&f.repository, &parse_args(&["--mode", "policy"]).unwrap());
        let mut bytes = Vec::new();
        write_result(&mut bytes, Some(Mode::Policy), &failure).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"schemaVersion":"1.0","mode":"policy","status":"failed",
            "error":{"code":"INVALID_METADATA","subject":"packaging/mastra/source-records.json"}})
        );
    }

    // Repository notices can share one exact path; casefold aliases are NOT that same path.
    #[test]
    fn distribution_repository_file_aliases_fail_before_hashing() {
        let shared = migrated_artifact_case();
        // The source record's requiredNotice and the original notice-input both name this
        // exact repository path. Deduplication must still accept their None/Some size refs.
        let report = evaluate(&shared.repository, &artifact_request(&shared)).unwrap();
        assert_eq!(serde_json::to_value(&report.status).unwrap(), "verified");
        assert_eq!((report.files_checked, report.inputs_checked, report.dependencies_checked), (7, 5, 1));

        for wrong_digest in [false, true] {
            let f = valid_artifact_case();
            let contract = std::fs::read(f.repository.join("contract.json")).unwrap();
            // Identical bytes make this construction portable to case-insensitive hosts too;
            // differently spelled references are unsafe even if the filesystem aliases them.
            put_bytes(&f.repository, "CONTRACT.JSON", &contract);
            rewrite_json(&f.artifact, "inventory.json", |i| {
                i["lockfile"] = json!({"path":"CONTRACT.JSON", "digest":digest(if wrong_digest {
                    b"hashing must not precede repository alias rejection".as_slice()
                } else { &contract })});
            });
            reapprove(&f);
            artifact_error(&f, &artifact_request(&f), "UNSAFE_PATH");
        }
    }

    // Different leaf names still share a directory identity: checking only full file
    // casefolds would miss these parent aliases in the sparse repository reference set.
    #[test]
    fn distribution_repository_parent_aliases_fail_before_hashing() {
        for (lock_path, contract_path) in
            [("Refs/anchor/fixture.lock", "refs/anchor/contract.json"), ("refs/Nested/fixture.lock", "refs/nested/contract.json")]
        {
            for wrong_digest in [false, true] {
                let f = valid_artifact_case();
                let lock = std::fs::read(f.repository.join("fixture.lock")).unwrap();
                let contract = std::fs::read(f.repository.join("contract.json")).unwrap();
                put_bytes(&f.repository, lock_path, &lock);
                put_bytes(&f.repository, contract_path, &contract);
                rewrite_json(&f.artifact, "inventory.json", |i| {
                    i["lockfile"] = json!({"path":lock_path, "digest":digest(if wrong_digest {
                        b"hashing must not precede parent alias rejection".as_slice()
                    } else { &lock })});
                    i["contractManifest"]["path"] = json!(contract_path);
                });
                reapprove(&f);
                artifact_error(&f, &artifact_request(&f), "UNSAFE_PATH");
            }
        }
    }

    // Removing artifact admission, trusted anchor comparisons or exact approval must fail this test.
    #[test]
    fn distribution_identity_and_trusted_anchors_must_match() {
        let f = valid_artifact_case();
        let request = artifact_request(&f);
        let report = evaluate(&f.repository, &request).expect("original synthetic artifact must be admitted");
        assert_eq!(report.mode, Mode::Artifact);
        assert_eq!(serde_json::to_value(&report.status).unwrap(), "verified");
        assert_eq!(report.tree_kind, Some(TreeKind::Package));
        assert_eq!((report.files_checked, report.inputs_checked, report.dependencies_checked), (5, 3, 1));
        assert_eq!(report.inventory_digest, Some(digest(&std::fs::read(f.artifact.join("inventory.json")).unwrap())));

        for field in ["tree", "inputs", "approval", "revision", "contract", "policy", "sources", "target", "advertised"] {
            let mut r = request.clone();
            match field {
                "tree" => r.tree = None,
                "inputs" => r.inputs_root = None,
                "approval" => r.approval_id = None,
                "revision" => r.expected_revision = None,
                "contract" => r.expected_contract_digest = None,
                "policy" => r.expected_policy_digest = None,
                "sources" => r.expected_sources_digest = None,
                "target" => r.target = None,
                "advertised" => r.advertised = None,
                _ => unreachable!(),
            }
            artifact_error(&f, &r, "INVALID_ARGUMENT");
        }
        let mut unsupported = request.clone();
        unsupported.target = Some("wasm32-unknown-unknown".into());
        artifact_error(&f, &unsupported, "UNSUPPORTED_TARGET");
        for flag in ["tree", "inputs"] {
            let mut r = request.clone();
            if flag == "tree" {
                r.tree = Some(f.root.join("absent"));
            } else {
                r.inputs_root = Some(f.root.join("absent"));
            }
            artifact_error(&f, &r, "IO_ERROR");
        }
        for path in ["runtime.json", "inventory.json"] {
            let f = valid_artifact_case();
            std::fs::remove_file(f.artifact.join(path)).unwrap();
            artifact_error(&f, &artifact_request(&f), "IO_ERROR");
        }
        for (pointer, replacement, want) in [
            ("/targetTriple", json!("aarch64-apple-darwin"), "IDENTITY_MISMATCH"),
            ("/repositoryRevision", json!("0000000000000000000000000000000000000000"), "IDENTITY_MISMATCH"),
            ("/runtimeVersion", json!("9.0.0"), "IDENTITY_MISMATCH"),
            ("/nodeVersion", json!("9.0.0"), "IDENTITY_MISMATCH"),
            ("/protocolVersions", json!([]), "INVALID_METADATA"),
            ("/policyDigest", json!(digest(b"wrong policy")), "DIGEST_MISMATCH"),
            ("/sourceRecordsDigest", json!(digest(b"wrong sources")), "DIGEST_MISMATCH"),
            ("/contractDigest", json!(digest(b"wrong contract")), "DIGEST_MISMATCH"),
            ("/provenanceInventory/digest", json!(digest(b"wrong inventory")), "DIGEST_MISMATCH"),
            ("/artifactDigests", json!([]), "DIGEST_MISMATCH"),
            ("/artifactDigests/0/digest", json!(digest(b"wrong output")), "DIGEST_MISMATCH"),
        ] {
            let f = valid_artifact_case();
            rewrite_json(&f.artifact, "runtime.json", |r| *r.pointer_mut(pointer).unwrap() = replacement);
            artifact_error(&f, &artifact_request(&f), want);
        }
        // Recomputed observed hashes do not approve producer changes, including manager/lock pins.
        for (pointer, replacement) in [
            ("/target", json!("aarch64-apple-darwin")),
            ("/runtimeVersion", json!("9.0.0")),
            ("/node/version", json!("9.0.0")),
            ("/node/archiveDigest", json!(digest(b"other Node archive"))),
            ("/packageManager/name", json!("pnpm")),
            ("/packageManager/version", json!("9.0.0")),
            ("/packageManager/archiveDigest", json!(digest(b"other manager archive"))),
            ("/lockfile/digest", json!(digest(b"other lock"))),
            ("/contractManifest/digest", json!(digest(b"other contract"))),
        ] {
            let f = valid_artifact_case();
            rewrite_json(&f.artifact, "inventory.json", |i| *i.pointer_mut(pointer).unwrap() = replacement);
            let observed = digest(&std::fs::read(f.artifact.join("inventory.json")).unwrap());
            rewrite_json(&f.artifact, "runtime.json", |r| r["provenanceInventory"]["digest"] = json!(observed));
            artifact_error(&f, &artifact_request(&f), "UNAPPROVED_ARTIFACT");
        }
        for path in ["fixture.lock", "contract.json"] {
            let f = valid_artifact_case();
            let request = artifact_request(&f);
            put_bytes(&f.repository, path, b"changed actual trusted checkout bytes");
            artifact_error(&f, &request, "DIGEST_MISMATCH");
        }
        for anchor in ["policy", "sources", "contract"] {
            let mut r = request.clone();
            let wrong = Some(digest(b"wrong trusted expectation"));
            match anchor {
                "policy" => r.expected_policy_digest = wrong,
                "sources" => r.expected_sources_digest = wrong,
                "contract" => r.expected_contract_digest = wrong,
                _ => unreachable!(),
            }
            artifact_error(&f, &r, "DIGEST_MISMATCH");
        }
        let f = valid_artifact_case();
        rewrite_json(&f.repository, "packaging/mastra/policy.json", |p| p["approvals"] = json!([]));
        artifact_error(&f, &artifact_request(&f), "UNAPPROVED_ARTIFACT");
    }

    // Repeating the current SHA inside approval-hashed Inventory must not reintroduce the cycle.
    #[test]
    fn distribution_current_revision_is_bound_by_receipt_and_ci() {
        let f = valid_artifact_case();
        assert!(fixture_json(&f.artifact, "inventory.json")["inputs"][1]["sourceRevision"].is_null());
        evaluate(&f.repository, &artifact_request(&f)).unwrap();
        let mut wrong_ci = artifact_request(&f);
        wrong_ci.expected_revision = Some("0000000000000000000000000000000000000000".into());
        artifact_error(&f, &wrong_ci, "IDENTITY_MISMATCH");
        for (field, value, want) in [
            ("sourceRevision", json!("1234567890abcdef1234567890abcdef12345678"), "IDENTITY_MISMATCH"),
            ("originId", json!("other"), "PROVENANCE_INCOMPLETE"),
            ("originalPath", json!("other.txt"), "IO_ERROR"),
        ] {
            let f = valid_artifact_case();
            rewrite_json(&f.artifact, "inventory.json", |i| i["inputs"][1][field] = value);
            reapprove(&f);
            artifact_error(&f, &artifact_request(&f), want);
        }
        for (path, field) in [("runtime.json", "repositoryRevision"), ("inventory.json", "sourceRevision")] {
            let f = valid_artifact_case();
            rewrite_json(&f.artifact, path, |v| {
                if path == "runtime.json" {
                    v.as_object_mut().unwrap().remove(field);
                } else {
                    v["inputs"][1].as_object_mut().unwrap().remove(field);
                }
            });
            artifact_error(&f, &artifact_request(&f), "INVALID_METADATA");
        }
        for root in ["repository", "inputs"] {
            let f = valid_artifact_case();
            put_bytes(if root == "repository" { &f.repository } else { &f.inputs }, "licenses/original.txt", b"changed");
            artifact_error(&f, &artifact_request(&f), "DIGEST_MISMATCH");
        }
        // Neither Policy nor Receipt is runtime build code, even if copied into a materialized input.
        for control in ["packaging/mastra/policy.json", "runtime.json"] {
            let f = valid_artifact_case();
            rewrite_json(&f.artifact, "inventory.json", |i| i["inputs"][1]["originalPath"] = json!(control));
            reapprove(&f);
            artifact_error(&f, &artifact_request(&f), "PROVENANCE_INCOMPLETE");
        }
        // Generated evidence is an actual hashed input, with identity outside Inventory too.
        let f = transformed_artifact_case();
        let report = evaluate(&f.repository, &artifact_request(&f)).unwrap();
        assert_eq!((report.files_checked, report.inputs_checked, report.dependencies_checked), (5, 6, 1));
        for (field, value, want) in [
            ("sourceRevision", json!("1234567890abcdef1234567890abcdef12345678"), "IDENTITY_MISMATCH"),
            ("originId", json!("wrong-evidence"), "PROVENANCE_INCOMPLETE"),
            ("originalPath", json!("wrong.json"), "PROVENANCE_INCOMPLETE"),
        ] {
            let f = transformed_artifact_case();
            rewrite_json(&f.artifact, "inventory.json", |i| i["inputs"][1][field] = value);
            reapprove(&f);
            artifact_error(&f, &artifact_request(&f), want);
        }
        let f = transformed_artifact_case();
        rewrite_json(&f.artifact, "inventory.json", |i| {
            i["inputs"][1].as_object_mut().unwrap().remove("sourceRevision");
        });
        artifact_error(&f, &artifact_request(&f), "INVALID_METADATA");
        let f = transformed_artifact_case();
        put_bytes(&f.inputs, "build/evidence.json", b"changed generated evidence bytes");
        artifact_error(&f, &artifact_request(&f), "DIGEST_MISMATCH");
    }

    fn transformed_artifact_case() -> DistributionFixture {
        let f = valid_artifact_case();
        let tool = b"Original inert synthetic tool; never execute.\n";
        let manager = b"Original inert synthetic manager; never execute.\n";
        let evidence = serde_json::to_vec(&json!({"schemaVersion":"1.0","outputPath":"dist/main.js",
            "inputIds":["code-input"],"toolInputIds":["manager-input","tool-input"]}))
        .unwrap();
        for (path, bytes) in
            [("build/evidence.json", evidence.as_slice()), ("tools/manager.js", manager.as_slice()), ("tools/tool.js", tool.as_slice())]
        {
            put_bytes(&f.inputs, path, bytes);
        }
        rewrite_json(&f.artifact, "inventory.json", |i| {
            let mut tool_dependency = i["dependencies"][0].clone();
            tool_dependency["id"] = json!("fixture-tool");
            tool_dependency["name"] = json!("@fixture/tool");
            tool_dependency["sourceUrl"] = json!("https://fixture.invalid/tool.git");
            i["rootToolIds"] = json!(["fixture-tool"]);
            i["buildTools"] = json!([tool_dependency]);
            for (id, path, origin, origin_id, bytes, revision) in [
                ("evidence-input", "build/evidence.json", "build-evidence", "evidence-input", evidence.as_slice(), None),
                ("manager-input", "tools/manager.js", "tool", "package-manager", manager.as_slice(), None),
                ("tool-input", "tools/tool.js", "tool", "fixture-tool", tool.as_slice(), Some("abcdef0123456789abcdef0123456789abcdef01")),
            ] {
                i["inputs"].as_array_mut().unwrap().push(json!({"id":id,"materializedPath":path,"originKind":origin,"originId":origin_id,
                    "originalPath":path,"sourceRevision":revision,"digest":digest(bytes),"sizeBytes":bytes.len(),
                    "licenseIds":["fixture-license"],"sourceRecordId":null}));
            }
            i["inputs"].as_array_mut().unwrap().sort_by_key(|v| v["id"].as_str().unwrap().to_owned());
            i["derivations"][0]["toolInputIds"] = json!(["manager-input", "tool-input"]);
            i["derivations"][0]["evidenceInputId"] = json!("evidence-input");
        });
        reapprove(&f);
        f
    }

    fn migrated_artifact_case() -> DistributionFixture {
        let f = valid_artifact_case();
        let upstream = "abcdef0123456789abcdef0123456789abcdef01";
        let adapted = b"export const fixture = 1;\n";
        let notice = b"Original synthetic retained source notice.\n";
        put_bytes(&f.repository, "src/adapted.ts", adapted);
        for root in [&f.repository, &f.inputs, &f.artifact] {
            put_bytes(root, "licenses/source-notice.txt", notice);
        }
        rewrite_json(&f.repository, "packaging/mastra/source-records.json", |s| {
            s["authorizations"][0]["sourceRevision"] = json!(upstream);
            let r = &mut s["records"][0];
            r["sourceRevision"] = json!(upstream);
            r["sourceDigest"] = json!(digest(b"distinct original synthetic upstream bytes"));
            r["destinationPath"] = json!("src/adapted.ts");
            r["destinationDigest"] = json!(digest(adapted));
            r["adaptation"] = json!("Original synthetic adaptation, not imported source");
            r["rightsHolder"] = json!("intgral-ai");
            r["licenseExpression"] = json!("MIT");
            r["requiredNotices"] = json!([{"path":"licenses/source-notice.txt","digest":digest(notice)}]);
            r["dependencyIds"] = json!(["fixture-dependency"]);
            r["thirdPartyReview"] = json!({"status":"verified","evidence":["Original synthetic independent third-party audit"]});
            r["verification"] = json!({"status":"verified","evidence":["Original synthetic per-file rights audit"]});
        });
        rewrite_json(&f.artifact, "inventory.json", |i| {
            let input = &mut i["inputs"][0];
            input["originKind"] = json!("repository");
            input["originId"] = json!("fixture-source");
            input["sourceRecordId"] = json!("fixture-source");
            input["sourceRevision"] = json!(upstream);
            input["originalPath"] = json!("src/original.ts");
            i["licenses"][0]["rightsHolder"] = json!("intgral-ai");
            i["licenses"][0]["noticeFiles"] = json!([{"path":"licenses/source-notice.txt","digest":digest(notice)}]);
            i["inputs"].as_array_mut().unwrap().push(json!({"id":"notice-input","materializedPath":"licenses/source-notice.txt",
                "originKind":"repository","originId":"notice-input","originalPath":"licenses/source-notice.txt","sourceRevision":null,
                "digest":digest(notice),"sizeBytes":notice.len(),"licenseIds":["fixture-license"],"sourceRecordId":null}));
            i["outputs"].as_array_mut().unwrap().insert(
                2,
                json!({"path":"licenses/source-notice.txt","digest":digest(notice),
                "sizeBytes":notice.len(),"role":"notice","licenseIds":["fixture-license"]}),
            );
            i["derivations"].as_array_mut().unwrap().insert(
                2,
                json!({"outputPath":"licenses/source-notice.txt",
                "inputIds":["notice-input"],"toolInputIds":[],"evidenceInputId":null}),
            );
        });
        let package = b"export const syntheticDependency = 2;\n";
        for root in [&f.inputs, &f.artifact] {
            put_bytes(root, "packages/fixture.js", package);
        }
        rewrite_json(&f.artifact, "inventory.json", |i| {
            i["inputs"].as_array_mut().unwrap().push(json!({"id":"package-input","materializedPath":"packages/fixture.js",
                "originKind":"dependency","originId":"fixture-dependency","originalPath":"lib/fixture.js","sourceRevision":null,
                "digest":digest(package),"sizeBytes":package.len(),"licenseIds":["fixture-license"],"sourceRecordId":null}));
            i["outputs"].as_array_mut().unwrap().push(json!({"path":"packages/fixture.js","digest":digest(package),
                "sizeBytes":package.len(),"role":"code","licenseIds":["fixture-license"]}));
            i["derivations"].as_array_mut().unwrap().push(json!({"outputPath":"packages/fixture.js","inputIds":["package-input"],
                "toolInputIds":[],"evidenceInputId":null}));
        });
        refresh_fixture_receipt(&f);
        f
    }

    fn remove_copied_dependency(f: &DistributionFixture, id: &str, path: &str) {
        rewrite_json(&f.artifact, "inventory.json", |i| {
            i["inputs"].as_array_mut().unwrap().retain(|v| v["id"] != id);
            i["outputs"].as_array_mut().unwrap().retain(|v| v["path"] != path);
            i["derivations"].as_array_mut().unwrap().retain(|v| v["outputPath"] != path);
        });
        for root in [&f.inputs, &f.artifact] {
            if root.join(path).exists() {
                std::fs::remove_file(root.join(path)).unwrap();
            }
        }
        refresh_fixture_receipt(f);
    }

    // SourceRecord.dependencyIds must never substitute for actual dependency bytes,
    // even when the migrated output is an exact single-input copy, not a bundle.
    #[test]
    fn distribution_migrated_copies_require_materialized_dependencies() {
        let f = migrated_artifact_case();
        // Before R1 the fixture already has no package-input/output; the removal is a
        // no-op and this observes the actual erroneous Verified result, not a hash error.
        remove_copied_dependency(&f, "package-input", "packages/fixture.js");
        artifact_error(&f, &artifact_request(&f), "PROVENANCE_INCOMPLETE");

        let f = migrated_artifact_case();
        let report = evaluate(&f.repository, &artifact_request(&f)).unwrap();
        assert_eq!((report.files_checked, report.inputs_checked, report.dependencies_checked), (7, 5, 1));
        let inventory = fixture_json(&f.artifact, "inventory.json");
        assert_eq!(inventory["derivations"][0]["inputIds"], json!(["code-input"]));
        assert_eq!(inventory["derivations"][0]["toolInputIds"], json!([]));
        assert!(inventory["derivations"][0]["evidenceInputId"].is_null());
        assert_eq!(inventory["derivations"][4]["inputIds"], json!(["package-input"]));
        let f = migrated_artifact_case();
        remove_copied_dependency(&f, "package-input", "packages/fixture.js");
        put_bytes(&f.artifact, "dist/main.js", b"closure rejection must precede this code hash mismatch");
        artifact_error(&f, &artifact_request(&f), "PROVENANCE_INCOMPLETE");
        for root in ["inputs", "artifact"] {
            let f = migrated_artifact_case();
            let base = if root == "inputs" { &f.inputs } else { &f.artifact };
            std::fs::remove_file(base.join("packages/fixture.js")).unwrap();
            artifact_error(&f, &artifact_request(&f), "FILE_SET_MISMATCH");
            put_bytes(base, "packages/fixture.js", b"changed required dependency bytes");
            artifact_error(&f, &artifact_request(&f), "DIGEST_MISMATCH");
        }
    }

    fn migrated_dependency_graph_case(cyclic: bool) -> DistributionFixture {
        let f = migrated_artifact_case();
        let bytes = b"export const syntheticTransitive = 3;\n";
        for root in [&f.inputs, &f.artifact] {
            put_bytes(root, "packages/transitive.js", bytes);
        }
        rewrite_json(&f.artifact, "inventory.json", |i| {
            let mut child = i["dependencies"][0].clone();
            child["id"] = json!("fixture-transitive");
            child["dependencyIds"] = if cyclic { json!(["fixture-dependency"]) } else { json!([]) };
            i["dependencies"][0]["dependencyIds"] = json!(["fixture-transitive"]);
            i["dependencies"].as_array_mut().unwrap().push(child);
            i["inputs"].as_array_mut().unwrap().push(json!({"id":"transitive-input","materializedPath":"packages/transitive.js",
                "originKind":"dependency","originId":"fixture-transitive","originalPath":"lib/transitive.js","sourceRevision":null,
                "digest":digest(bytes),"sizeBytes":bytes.len(),"licenseIds":["fixture-license"],"sourceRecordId":null}));
            i["outputs"].as_array_mut().unwrap().push(json!({"path":"packages/transitive.js","digest":digest(bytes),
                "sizeBytes":bytes.len(),"role":"code","licenseIds":["fixture-license"]}));
            i["derivations"].as_array_mut().unwrap().push(json!({"outputPath":"packages/transitive.js","inputIds":["transitive-input"],
                "toolInputIds":[],"evidenceInputId":null}));
        });
        refresh_fixture_receipt(&f);
        f
    }

    // A source-required owner closure needs actual input bytes even through a closed
    // dependency cycle; a different real owner or another record ID is not that input.
    #[test]
    fn distribution_migrated_dependency_cycles_require_exact_owner_bytes() {
        for cyclic in [false, true] {
            let f = migrated_dependency_graph_case(cyclic);
            let report = evaluate(&f.repository, &artifact_request(&f)).unwrap();
            assert_eq!((report.files_checked, report.inputs_checked, report.dependencies_checked), (8, 6, 2));
            for (id, path) in [("package-input", "packages/fixture.js"), ("transitive-input", "packages/transitive.js")] {
                let f = migrated_dependency_graph_case(cyclic);
                remove_copied_dependency(&f, id, path);
                artifact_error(&f, &artifact_request(&f), "PROVENANCE_INCOMPLETE");
                put_bytes(&f.artifact, "dist/main.js", b"do not hash code before source-required owner closure");
                artifact_error(&f, &artifact_request(&f), "PROVENANCE_INCOMPLETE");
            }
            for owner in ["fixture-transitive", "missing-dependency"] {
                let f = migrated_dependency_graph_case(cyclic);
                rewrite_json(&f.artifact, "inventory.json", |i| {
                    let input = i["inputs"].as_array_mut().unwrap().iter_mut().find(|v| v["id"] == "package-input").unwrap();
                    input["originId"] = json!(owner);
                });
                reapprove(&f);
                artifact_error(&f, &artifact_request(&f), "PROVENANCE_INCOMPLETE");
            }
            let f = migrated_dependency_graph_case(cyclic);
            rewrite_json(&f.artifact, "inventory.json", |i| i["dependencies"][1]["dependencyIds"] = json!(["missing-transitive"]));
            reapprove(&f);
            artifact_error(&f, &artifact_request(&f), "PROVENANCE_INCOMPLETE");
            for path in ["packages/fixture.js", "packages/transitive.js"] {
                for root in ["inputs", "artifact"] {
                    let f = migrated_dependency_graph_case(cyclic);
                    let base = if root == "inputs" { &f.inputs } else { &f.artifact };
                    std::fs::remove_file(base.join(path)).unwrap();
                    artifact_error(&f, &artifact_request(&f), "FILE_SET_MISMATCH");
                    put_bytes(base, path, b"changed required dependency bytes");
                    artifact_error(&f, &artifact_request(&f), "DIGEST_MISMATCH");
                }
            }
        }
    }

    // Nulling all repository revisions or substituting the current SHA loses reviewed upstream identity.
    #[test]
    fn distribution_external_revision_is_not_replaced_by_current_sha() {
        let f = migrated_artifact_case();
        evaluate(&f.repository, &artifact_request(&f)).unwrap();
        let i = fixture_json(&f.artifact, "inventory.json");
        assert_ne!(i["inputs"][0]["sourceRevision"], json!(artifact_request(&f).expected_revision));
        for value in [Value::Null, json!("0000000000000000000000000000000000000000"), json!("1234567890abcdef1234567890abcdef12345678")] {
            let f = migrated_artifact_case();
            rewrite_json(&f.artifact, "inventory.json", |i| i["inputs"][0]["sourceRevision"] = value);
            reapprove(&f);
            artifact_error(&f, &artifact_request(&f), "SOURCE_RIGHTS_UNVERIFIED");
        }
        for (field, value) in [
            ("originalPath", json!("src/wrong.ts")),
            ("originId", json!("fixture-dependency")),
            ("digest", json!(digest(b"upstream is not adapted bytes"))),
        ] {
            let f = migrated_artifact_case();
            rewrite_json(&f.artifact, "inventory.json", |i| i["inputs"][0][field] = value);
            reapprove(&f);
            artifact_error(&f, &artifact_request(&f), "SOURCE_RIGHTS_UNVERIFIED");
        }
        let f = migrated_artifact_case();
        put_bytes(&f.repository, "src/adapted.ts", b"wrong actual checkout adaptation");
        artifact_error(&f, &artifact_request(&f), "DIGEST_MISMATCH");
        // External pins are approval-bound; a producer cannot null/replace them by rewriting observations.
        let f = valid_artifact_case();
        rewrite_json(&f.artifact, "inventory.json", |i| {
            i["dependencies"][0]["sourceUrl"] = json!("https://fixture.invalid/dependency.git");
            i["inputs"][0]["sourceRevision"] = json!("abcdef0123456789abcdef0123456789abcdef01");
        });
        reapprove(&f);
        evaluate(&f.repository, &artifact_request(&f)).unwrap();
        let inventory = std::fs::read(f.artifact.join("inventory.json")).unwrap();
        for replacement in [Value::Null, json!("1234567890abcdef1234567890abcdef12345678")] {
            put_bytes(&f.artifact, "inventory.json", &inventory);
            rewrite_json(&f.artifact, "inventory.json", |i| i["inputs"][0]["sourceRevision"] = replacement);
            artifact_error(&f, &artifact_request(&f), "UNAPPROVED_ARTIFACT");
        }
        let f = transformed_artifact_case();
        evaluate(&f.repository, &artifact_request(&f)).unwrap();
        let inventory = std::fs::read(f.artifact.join("inventory.json")).unwrap();
        for replacement in [Value::Null, json!("1234567890abcdef1234567890abcdef12345678")] {
            put_bytes(&f.artifact, "inventory.json", &inventory);
            rewrite_json(&f.artifact, "inventory.json", |i| i["inputs"][5]["sourceRevision"] = replacement);
            artifact_error(&f, &artifact_request(&f), "UNAPPROVED_ARTIFACT");
        }
        // No Git SHA is invented for release archives; exact version/archive/integrity remain required.
        for (pointer, value) in [
            ("/dependencies/0/version", json!("latest")),
            ("/dependencies/0/archiveDigest", Value::Null),
            ("/dependencies/0/lockIntegrity", json!("")),
            ("/packageManager/version", json!("^1.0.0")),
            ("/node/archiveDigest", Value::Null),
        ] {
            let f = valid_artifact_case();
            rewrite_json(&f.artifact, "inventory.json", |i| *i.pointer_mut(pointer).unwrap() = value);
            reapprove(&f);
            artifact_error(&f, &artifact_request(&f), "INVALID_METADATA");
        }
    }

    // Exact hashes alone do not close extra files, dangling evidence, license roles or dependency graphs.
    #[test]
    fn distribution_extra_missing_and_changed_files_fail() {
        let f = valid_artifact_case();
        evaluate(&f.repository, &artifact_request(&f)).expect("copied original synthetic outputs must be admitted");
        for root in ["artifact", "inputs"] {
            for extra in ["injected.js", "target/cache.js", "node_modules/hidden.js", ".git/hidden"] {
                let f = valid_artifact_case();
                put_bytes(if root == "artifact" { &f.artifact } else { &f.inputs }, extra, b"undeclared synthetic bytes");
                artifact_error(&f, &artifact_request(&f), "FILE_SET_MISMATCH");
            }
            for path in ["dist/main.js", "node", "licenses/original.txt"] {
                let f = valid_artifact_case();
                let base = if root == "artifact" { &f.artifact } else { &f.inputs };
                std::fs::remove_file(base.join(path)).unwrap();
                artifact_error(&f, &artifact_request(&f), "FILE_SET_MISMATCH");
                put_bytes(base, path, b"changed actual bytes");
                artifact_error(&f, &artifact_request(&f), "DIGEST_MISMATCH");
            }
        }
        for path in ["inventory.json", "runtime.json"] {
            let f = valid_artifact_case();
            rewrite_json(&f.artifact, path, |v| v["payload"] = json!("hidden code is not a control object field"));
            artifact_error(&f, &artifact_request(&f), "INVALID_METADATA");
        }
        for (pointer, value, want) in [
            ("/licenses/0/expression", json!("Unknown"), "SOURCE_RIGHTS_UNVERIFIED"),
            ("/licenses/0/licenseFiles/0/digest", json!(digest(b"wrong license bytes")), "PROVENANCE_INCOMPLETE"),
            ("/licenses/0/licenseFiles/0/path", json!("dist/main.js"), "PROVENANCE_INCOMPLETE"),
            ("/outputs/0/role", json!("notice"), "PROVENANCE_INCOMPLETE"),
            ("/outputs/1/role", json!("data"), "PROVENANCE_INCOMPLETE"),
            ("/outputs/0/licenseIds", json!(["missing-license"]), "PROVENANCE_INCOMPLETE"),
            ("/inputs/0/licenseIds", json!(["missing-license"]), "PROVENANCE_INCOMPLETE"),
            ("/inputs/0/originId", json!("missing-dependency"), "PROVENANCE_INCOMPLETE"),
            ("/rootDependencyIds", json!([]), "PROVENANCE_INCOMPLETE"),
            ("/dependencies/0/dependencyIds", json!(["missing-transitive"]), "PROVENANCE_INCOMPLETE"),
            ("/derivations/0/inputIds", json!(["missing-input"]), "PROVENANCE_INCOMPLETE"),
            ("/derivations/0/inputIds", json!(["license-input"]), "PROVENANCE_INCOMPLETE"),
            ("/derivations", json!([]), "PROVENANCE_INCOMPLETE"),
            ("/node/executablePath", json!("dist/main.js"), "PROVENANCE_INCOMPLETE"),
        ] {
            let f = valid_artifact_case();
            rewrite_json(&f.artifact, "inventory.json", |i| *i.pointer_mut(pointer).unwrap() = value);
            refresh_fixture_receipt(&f);
            artifact_error(&f, &artifact_request(&f), want);
        }
        for registry in ["dependencies", "licenses", "inputs", "buildTools"] {
            let f = valid_artifact_case();
            rewrite_json(&f.artifact, "inventory.json", |i| {
                let mut unused = if registry == "buildTools" { i["dependencies"][0].clone() } else { i[registry][0].clone() };
                unused["id"] = json!("zz-orphan");
                if registry == "inputs" {
                    unused["materializedPath"] = json!("unused.txt");
                }
                i[registry].as_array_mut().unwrap().push(unused);
            });
            refresh_fixture_receipt(&f);
            artifact_error(&f, &artifact_request(&f), "PROVENANCE_INCOMPLETE");
        }
        let f = migrated_artifact_case();
        std::fs::remove_file(f.artifact.join("licenses/source-notice.txt")).unwrap();
        artifact_error(&f, &artifact_request(&f), "FILE_SET_MISMATCH");
        let f = valid_artifact_case();
        // Unknown rights must fail before the later hash mismatch in actual code bytes.
        put_bytes(&f.artifact, "dist/main.js", b"wrong code");
        rewrite_json(&f.artifact, "inventory.json", |i| i["licenses"][0]["expression"] = json!("Unknown"));
        reapprove(&f);
        artifact_error(&f, &artifact_request(&f), "SOURCE_RIGHTS_UNVERIFIED");
    }

    // Counting legal files as the only use of a license, or roots as the only use of a tool,
    // would let a closed-looking inventory advertise uninspected/orphaned registries.
    #[test]
    fn distribution_owner_registries_require_real_consumers() {
        let f = valid_artifact_case();
        rewrite_json(&f.artifact, "inventory.json", |i| {
            let mut license = i["licenses"][0].clone();
            license["id"] = json!("zz-orphan");
            i["licenses"].as_array_mut().unwrap().push(license);
            i["inputs"][1]["licenseIds"] = json!(["fixture-license", "zz-orphan"]);
            i["outputs"][1]["licenseIds"] = json!(["fixture-license", "zz-orphan"]);
        });
        refresh_fixture_receipt(&f);
        artifact_error(&f, &artifact_request(&f), "PROVENANCE_INCOMPLETE");
        let f = valid_artifact_case();
        rewrite_json(&f.artifact, "inventory.json", |i| {
            let mut tool = i["dependencies"][0].clone();
            tool["id"] = json!("fixture-tool");
            i["buildTools"] = json!([tool]);
            i["rootToolIds"] = json!(["fixture-tool"]);
        });
        reapprove(&f);
        artifact_error(&f, &artifact_request(&f), "PROVENANCE_INCOMPLETE");
        let f = valid_artifact_case();
        put_bytes(&f.repository, "dist/main.js", b"export const fixture = 1;\n");
        rewrite_json(&f.artifact, "inventory.json", |i| {
            i["inputs"][0]["originKind"] = json!("repository");
            i["inputs"][0]["originId"] = json!("code-input");
        });
        reapprove(&f);
        artifact_error(&f, &artifact_request(&f), "PROVENANCE_INCOMPLETE");
    }

    // An identity error must win over later rights/file errors; an unapproved producer cannot
    // gain admission by changing license claims or by recomputing the receipt's checksum list.
    #[test]
    fn distribution_admission_phases_reject_before_payload_hashing() {
        let f = valid_artifact_case();
        rewrite_json(&f.artifact, "inventory.json", |i| {
            i["inputs"][1]["sourceRevision"] = json!("1234567890abcdef1234567890abcdef12345678");
            i["licenses"][0]["expression"] = json!("Unknown");
        });
        reapprove(&f);
        artifact_error(&f, &artifact_request(&f), "IDENTITY_MISMATCH");
        let f = valid_artifact_case();
        rewrite_json(&f.artifact, "inventory.json", |i| i["licenses"][0]["expression"] = json!("Unknown"));
        artifact_error(&f, &artifact_request(&f), "UNAPPROVED_ARTIFACT");
        let f = valid_artifact_case();
        rewrite_json(&f.artifact, "inventory.json", |i| i["inputs"][0]["originalPath"] = json!("src/EE/hidden.ts"));
        artifact_error(&f, &artifact_request(&f), "ENTERPRISE_INPUT");

        // Even an explicit synthetic reapproval cannot turn a package license string into
        // actual license evidence or erase a verified source's required notice.
        let f = valid_artifact_case();
        rewrite_json(&f.artifact, "inventory.json", |i| {
            i["licenses"][0]["expression"] = json!("Apache-2.0");
            i["licenses"][0]["licenseFiles"] = json!([]);
        });
        reapprove(&f);
        artifact_error(&f, &artifact_request(&f), "INVALID_METADATA");
        let f = migrated_artifact_case();
        rewrite_json(&f.artifact, "inventory.json", |i| {
            i["licenses"][0]["noticeFiles"] = json!([]);
            i["outputs"].as_array_mut().unwrap().remove(2);
            i["derivations"].as_array_mut().unwrap().remove(2);
            i["inputs"].as_array_mut().unwrap().retain(|v| v["id"] != "notice-input");
        });
        std::fs::remove_file(f.artifact.join("licenses/source-notice.txt")).unwrap();
        std::fs::remove_file(f.inputs.join("licenses/source-notice.txt")).unwrap();
        refresh_fixture_receipt(&f);
        artifact_error(&f, &artifact_request(&f), "SOURCE_RIGHTS_UNVERIFIED");
        for control in ["runtime.json", "inventory.json"] {
            let f = valid_artifact_case();
            rewrite_json(&f.artifact, "inventory.json", |i| {
                i["outputs"].as_array_mut().unwrap().push(json!({"path":control,"digest":digest(b"not a self-hash"),
                    "sizeBytes":15,"role":"data","licenseIds":["fixture-license"]}));
                i["outputs"].as_array_mut().unwrap().sort_by_key(|o| o["path"].as_str().unwrap().to_owned());
            });
            refresh_fixture_receipt(&f);
            artifact_error(&f, &artifact_request(&f), "PROVENANCE_INCOMPLETE");
        }
        #[cfg(unix)]
        {
            let f = valid_artifact_case();
            std::fs::remove_file(f.repository.join("licenses/original.txt")).unwrap();
            std::os::unix::fs::symlink(f.root.join("outside-unreadable"), f.repository.join("licenses/original.txt")).unwrap();
            artifact_error(&f, &artifact_request(&f), "UNSAFE_PATH");
        }
    }

    fn replace_fixture_evidence(f: &DistributionFixture, bytes: &[u8]) {
        put_bytes(&f.inputs, "build/evidence.json", bytes);
        rewrite_json(&f.artifact, "inventory.json", |i| {
            let input = i["inputs"].as_array_mut().unwrap().iter_mut().find(|v| v["id"] == "evidence-input").unwrap();
            input["digest"] = json!(digest(bytes));
            input["sizeBytes"] = json!(bytes.len());
        });
        reapprove(f);
    }

    // Two closed owner cycles, with every materialized input accounted for. The second
    // data file also has a copied output, so omitting it from the bundle isn't an orphan test.
    fn cyclic_artifact_case() -> DistributionFixture {
        let f = transformed_artifact_case();
        let data = b"Original synthetic transitive module.\n";
        let tool = b"Original inert synthetic transitive tool.\n";
        put_bytes(&f.inputs, "src/transitive.js", data);
        put_bytes(&f.artifact, "dist/other.js", data);
        put_bytes(&f.inputs, "tools/child.js", tool);
        rewrite_json(&f.artifact, "inventory.json", |i| {
            for (registry, child_id, parent_id) in
                [("dependencies", "fixture-transitive", "fixture-dependency"), ("buildTools", "fixture-tool-child", "fixture-tool")]
            {
                let mut child = i[registry][0].clone();
                child["id"] = json!(child_id);
                child["dependencyIds"] = json!([parent_id]);
                i[registry][0]["dependencyIds"] = json!([child_id]);
                i[registry].as_array_mut().unwrap().push(child);
            }
            for (id, path, origin, owner, bytes) in [
                ("transitive-input", "src/transitive.js", "dependency", "fixture-transitive", data.as_slice()),
                ("tool-child-input", "tools/child.js", "tool", "fixture-tool-child", tool.as_slice()),
            ] {
                i["inputs"].as_array_mut().unwrap().push(json!({"id":id,"materializedPath":path,"originKind":origin,"originId":owner,
                    "originalPath":path,"sourceRevision":null,"digest":digest(bytes),"sizeBytes":bytes.len(),
                    "licenseIds":["fixture-license"],"sourceRecordId":null}));
            }
            i["inputs"].as_array_mut().unwrap().sort_by_key(|v| v["id"].as_str().unwrap().to_owned());
            i["outputs"].as_array_mut().unwrap().insert(
                1,
                json!({"path":"dist/other.js","digest":digest(data),
                "sizeBytes":data.len(),"role":"data","licenseIds":["fixture-license"]}),
            );
            i["derivations"][0]["inputIds"] = json!(["code-input", "transitive-input"]);
            i["derivations"][0]["toolInputIds"] = json!(["manager-input", "tool-child-input", "tool-input"]);
            i["derivations"].as_array_mut().unwrap().insert(
                1,
                json!({"outputPath":"dist/other.js","inputIds":["transitive-input"],
                "toolInputIds":[],"evidenceInputId":null}),
            );
        });
        replace_fixture_evidence(&f, br#"{"schemaVersion":"1.0","outputPath":"dist/main.js","inputIds":["code-input","transitive-input"],"toolInputIds":["manager-input","tool-child-input","tool-input"]}"#);
        refresh_fixture_receipt(&f);
        f
    }

    // Removing the original-path exclusion, or letting approval override it, launders renamed bytes.
    #[test]
    fn distribution_enterprise_inputs_cannot_be_laundered() {
        for location in ["delivered", "original", "materialized", "transitive"] {
            let f = cyclic_artifact_case();
            // Deliberately wrong/minified payload must not be opened before the ee path veto.
            put_bytes(&f.artifact, "dist/main.js", b"(()=>0)();");
            match location {
                "delivered" => put_bytes(&f.artifact, "renamed/EE/bundle.js", b"(()=>0)();"),
                "materialized" => {
                    let bytes = std::fs::read(f.inputs.join("dist/main.js")).unwrap();
                    put_bytes(&f.inputs, "build/ee/bundle.js", &bytes);
                    std::fs::remove_file(f.inputs.join("dist/main.js")).unwrap();
                    rewrite_json(&f.artifact, "inventory.json", |i| i["inputs"][0]["materializedPath"] = json!("build/ee/bundle.js"));
                }
                "original" => rewrite_json(&f.artifact, "inventory.json", |i| i["inputs"][0]["originalPath"] = json!("package/ee/secret.ts")),
                "transitive" => rewrite_json(&f.artifact, "inventory.json", |i| {
                    let input = i["inputs"].as_array_mut().unwrap().iter_mut().find(|v| v["id"] == "transitive-input").unwrap();
                    input["originalPath"] = json!("package/Ee/secret.ts");
                }),
                _ => unreachable!(),
            }
            artifact_error(&f, &artifact_request(&f), "ENTERPRISE_INPUT");
            reapprove(&f);
            artifact_error(&f, &artifact_request(&f), "ENTERPRISE_INPUT");
        }
        // Even a coherent renamed/minified output with updated exact hashes and explicit
        // synthetic approval retains its prohibited original input path.
        let f = transformed_artifact_case();
        let minified = b"export const fixture=1;";
        std::fs::remove_file(f.artifact.join("dist/main.js")).unwrap();
        put_bytes(&f.artifact, "dist/bundle.js", minified);
        rewrite_json(&f.artifact, "inventory.json", |i| {
            i["inputs"][0]["originalPath"] = json!("package/ee/original.ts");
            i["outputs"][0]["path"] = json!("dist/bundle.js");
            i["outputs"][0]["digest"] = json!(digest(minified));
            i["outputs"][0]["sizeBytes"] = json!(minified.len());
            i["derivations"][0]["outputPath"] = json!("dist/bundle.js");
        });
        replace_fixture_evidence(
            &f,
            br#"{"schemaVersion":"1.0","outputPath":"dist/bundle.js","inputIds":["code-input"],"toolInputIds":["manager-input","tool-input"]}"#,
        );
        refresh_fixture_receipt(&f);
        artifact_error(&f, &artifact_request(&f), "ENTERPRISE_INPUT");
        // A non-ee observation is still not an independent approval, even when rehashed.
        let f = transformed_artifact_case();
        rewrite_json(&f.artifact, "inventory.json", |i| i["inputs"][0]["originalPath"] = json!("unreviewed/module.ts"));
        let observed = digest(&std::fs::read(f.artifact.join("inventory.json")).unwrap());
        rewrite_json(&f.artifact, "runtime.json", |r| r["provenanceInventory"]["digest"] = json!(observed));
        artifact_error(&f, &artifact_request(&f), "UNAPPROVED_ARTIFACT");

        let f = cyclic_artifact_case();
        rewrite_json(&f.artifact, "inventory.json", |i| {
            i["inputs"].as_array_mut().unwrap().retain(|v| v["id"] != "transitive-input");
        });
        std::fs::remove_file(f.inputs.join("src/transitive.js")).unwrap();
        reapprove(&f);
        artifact_error(&f, &artifact_request(&f), "PROVENANCE_INCOMPLETE");
    }

    // Owner authorization is not a per-file review or permission to substitute third-party rights.
    #[test]
    fn distribution_unverified_migration_is_not_admitted() {
        let f = migrated_artifact_case();
        let report = evaluate(&f.repository, &artifact_request(&f)).unwrap();
        assert_eq!((report.files_checked, report.inputs_checked, report.dependencies_checked), (7, 5, 1));
        for (field, value) in [
            ("sourceRecordId", json!("unknown-record")),
            ("originalPath", json!("src/other.ts")),
            ("sourceRevision", json!("1234567890abcdef1234567890abcdef12345678")),
            ("digest", json!(digest(b"not the reviewed adapted bytes"))),
        ] {
            let f = migrated_artifact_case();
            rewrite_json(&f.artifact, "inventory.json", |i| i["inputs"][0][field] = value);
            reapprove(&f);
            artifact_error(&f, &artifact_request(&f), "SOURCE_RIGHTS_UNVERIFIED");
        }
        for review in ["verification", "thirdPartyReview"] {
            let f = migrated_artifact_case();
            rewrite_json(&f.repository, "packaging/mastra/source-records.json", |s| {
                s["records"][0][review] = json!({"status":"unverified","evidence":[]});
            });
            refresh_fixture_receipt(&f);
            artifact_error(&f, &artifact_request(&f), "SOURCE_RIGHTS_UNVERIFIED");
        }
        let f = migrated_artifact_case();
        rewrite_json(&f.artifact, "inventory.json", |i| i["licenses"][0]["expression"] = json!("MIT OR Apache-2.0"));
        reapprove(&f);
        artifact_error(&f, &artifact_request(&f), "SOURCE_RIGHTS_UNVERIFIED");

        let f = migrated_artifact_case();
        rewrite_json(&f.artifact, "inventory.json", |i| {
            i["licenses"][0]["noticeFiles"] = json!([]);
            i["outputs"].as_array_mut().unwrap().remove(2);
            i["derivations"].as_array_mut().unwrap().remove(2);
            i["inputs"].as_array_mut().unwrap().retain(|v| v["id"] != "notice-input");
        });
        std::fs::remove_file(f.artifact.join("licenses/source-notice.txt")).unwrap();
        std::fs::remove_file(f.inputs.join("licenses/source-notice.txt")).unwrap();
        artifact_error(&f, &artifact_request(&f), "UNAPPROVED_ARTIFACT");
        refresh_fixture_receipt(&f);
        artifact_error(&f, &artifact_request(&f), "SOURCE_RIGHTS_UNVERIFIED");
    }

    // Skipping the hashed strict evidence projection, accepting a subset, or closing owner
    // graphs only globally (not for each bundle) must break these real evaluator assertions.
    #[test]
    fn distribution_bundle_evidence_must_cover_every_input() {
        for (f, counts) in [(valid_artifact_case(), (5, 3, 1)), (transformed_artifact_case(), (5, 6, 1)), (cyclic_artifact_case(), (6, 8, 2))] {
            let report = evaluate(&f.repository, &artifact_request(&f)).unwrap();
            assert_eq!(report.status, GateStatus::Verified);
            assert_eq!((report.files_checked, report.inputs_checked, report.dependencies_checked), counts);
        }
        for (pointer, replacement) in [
            ("/derivations/0/evidenceInputId", Value::Null),
            ("/derivations/0/evidenceInputId", json!("license-input")),
            ("/derivations/0/toolInputIds", json!(["missing-tool"])),
            ("/derivations/0/inputIds", json!(["missing-input"])),
            ("/derivations/0/inputIds", json!(["evidence-input"])),
            ("/derivations/0/toolInputIds", json!(["evidence-input"])),
            ("/derivations", json!([])),
            ("/dependencies/0/dependencyIds", json!(["missing-transitive"])),
            ("/buildTools/0/dependencyIds", json!(["missing-tool-transitive"])),
        ] {
            let f = transformed_artifact_case();
            rewrite_json(&f.artifact, "inventory.json", |i| *i.pointer_mut(pointer).unwrap() = replacement);
            reapprove(&f);
            artifact_error(&f, &artifact_request(&f), "PROVENANCE_INCOMPLETE");
        }
        for (field, replacement) in [
            ("outputPath", json!("dist/other.js")),
            ("inputIds", json!([])),
            ("inputIds", json!(["code-input", "license-input"])),
            ("inputIds", json!(["code-input", "evidence-input"])),
            ("toolInputIds", json!(["tool-input"])),
            ("toolInputIds", json!(["manager-input", "missing-tool", "tool-input"])),
        ] {
            let f = transformed_artifact_case();
            let mut evidence = fixture_json(&f.inputs, "build/evidence.json");
            evidence[field] = replacement;
            replace_fixture_evidence(&f, &serde_json::to_vec(&evidence).unwrap());
            artifact_error(&f, &artifact_request(&f), "PROVENANCE_INCOMPLETE");
        }
        for bytes in [
            br#"{"schemaVersion":"1.0","outputPath":"dist/main.js","inputIds":["code-input"],"toolInputIds":[],"payload":"hidden code"}"#.as_slice(),
            br#"{"schemaVersion":"1.0","outputPath":"dist/main.js","inputIds":["code-input"],"inputIds":["code-input"],"toolInputIds":[]}"#,
            br#"{"schemaVersion":"1.0","outputPath":"dist/main.js","inputIds":["code-input"]}"#,
            br#"{"schemaVersion":"2.0","outputPath":"dist/main.js","inputIds":["code-input"],"toolInputIds":[]}"#,
            br#"{"schemaVersion":"1.0","outputPath":"dist/main.js","inputIds":["code-input","code-input"],"toolInputIds":[]}"#,
            br#"{"schemaVersion":"1.0","outputPath":"dist/main.js","inputIds":["code-input"],"toolInputIds":["tool-input","manager-input"]}"#,
            b"export const notEvidence = 1;",
        ] {
            let f = transformed_artifact_case();
            replace_fixture_evidence(&f, bytes);
            artifact_error(&f, &artifact_request(&f), "INVALID_METADATA");
        }
        for key in ["schemaVersion", "outputPath", "inputIds", "toolInputIds"] {
            let f = transformed_artifact_case();
            let mut evidence = fixture_json(&f.inputs, "build/evidence.json");
            let duplicate = format!("{{{}:{},{}", serde_json::to_string(key).unwrap(), evidence[key], &evidence.to_string()[1..]);
            replace_fixture_evidence(&f, duplicate.as_bytes());
            artifact_error(&f, &artifact_request(&f), "INVALID_METADATA");
            evidence.as_object_mut().unwrap().remove(key);
            replace_fixture_evidence(&f, &serde_json::to_vec(&evidence).unwrap());
            artifact_error(&f, &artifact_request(&f), "INVALID_METADATA");
        }
        let f = transformed_artifact_case();
        let mut evidence = fixture_json(&f.inputs, "build/evidence.json");
        evidence["inputIds"] = json!(vec!["code-input"; 100_001]);
        replace_fixture_evidence(&f, &serde_json::to_vec(&evidence).unwrap());
        artifact_error(&f, &artifact_request(&f), "LIMIT_EXCEEDED");
        // Exact declared/evidence agreement alone isn't complete transitive provenance:
        // the omitted data input is still consumed by dist/other.js, hence not orphaned.
        let f = cyclic_artifact_case();
        rewrite_json(&f.artifact, "inventory.json", |i| i["derivations"][0]["inputIds"] = json!(["code-input"]));
        replace_fixture_evidence(&f, br#"{"schemaVersion":"1.0","outputPath":"dist/main.js","inputIds":["code-input"],"toolInputIds":["manager-input","tool-child-input","tool-input"]}"#);
        artifact_error(&f, &artifact_request(&f), "PROVENANCE_INCOMPLETE");

        // The same per-output requirement applies to tools, not only data owners. All
        // omitted tool bytes remain globally consumed by a second transformed output.
        let f = cyclic_artifact_case();
        let other = br#"{"schemaVersion":"1.0","outputPath":"dist/other.js","inputIds":["transitive-input"],"toolInputIds":["tool-child-input"]}"#;
        put_bytes(&f.inputs, "build/other.json", other);
        rewrite_json(&f.artifact, "inventory.json", |i| {
            // The second output needs only the child owners, not the original cycle.
            i["dependencies"][1]["dependencyIds"] = json!([]);
            i["buildTools"][1]["dependencyIds"] = json!([]);
            i["inputs"].as_array_mut().unwrap().push(json!({"id":"other-evidence-input","materializedPath":"build/other.json",
                "originKind":"build-evidence","originId":"other-evidence-input","originalPath":"build/other.json","sourceRevision":null,
                "digest":digest(other),"sizeBytes":other.len(),"licenseIds":["fixture-license"],"sourceRecordId":null}));
            i["inputs"].as_array_mut().unwrap().sort_by_key(|v| v["id"].as_str().unwrap().to_owned());
            i["derivations"][1]["toolInputIds"] = json!(["tool-child-input"]);
            i["derivations"][1]["evidenceInputId"] = json!("other-evidence-input");
        });
        reapprove(&f);
        let report = evaluate(&f.repository, &artifact_request(&f)).unwrap();
        assert_eq!((report.files_checked, report.inputs_checked, report.dependencies_checked), (6, 9, 2));
        rewrite_json(&f.artifact, "inventory.json", |i| i["derivations"][0]["toolInputIds"] = json!(["manager-input", "tool-input"]));
        replace_fixture_evidence(&f, br#"{"schemaVersion":"1.0","outputPath":"dist/main.js","inputIds":["code-input","transitive-input"],"toolInputIds":["manager-input","tool-input"]}"#);
        artifact_error(&f, &artifact_request(&f), "PROVENANCE_INCOMPLETE");

        // A verified source's dependency IDs are requirements, not substitutes for
        // materialized bundle inputs. Preserve its upstream pin and retained notice.
        let f = migrated_artifact_case();
        let build = transformed_artifact_case();
        let built = fixture_json(&build.artifact, "inventory.json");
        let tools: Vec<_> = built["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| matches!(v["originKind"].as_str(), Some("tool" | "build-evidence")))
            .cloned()
            .collect();
        for input in &tools {
            let path = input["materializedPath"].as_str().unwrap();
            put_bytes(&f.inputs, path, &std::fs::read(build.inputs.join(path)).unwrap());
        }
        let dependency = b"Original synthetic materialized source dependency.\n";
        put_bytes(&f.inputs, "src/dependency.js", dependency);
        rewrite_json(&f.artifact, "inventory.json", |i| {
            i["rootToolIds"] = built["rootToolIds"].clone();
            i["buildTools"] = built["buildTools"].clone();
            i["inputs"].as_array_mut().unwrap().extend(tools);
            i["inputs"].as_array_mut().unwrap().push(json!({"id":"dependency-input","materializedPath":"src/dependency.js",
                "originKind":"dependency","originId":"fixture-dependency","originalPath":"src/dependency.js","sourceRevision":null,
                "digest":digest(dependency),"sizeBytes":dependency.len(),"licenseIds":["fixture-license"],"sourceRecordId":null}));
            i["inputs"].as_array_mut().unwrap().sort_by_key(|v| v["id"].as_str().unwrap().to_owned());
            i["derivations"][0]["inputIds"] = json!(["code-input", "dependency-input"]);
            i["derivations"][0]["toolInputIds"] = json!(["manager-input", "tool-input"]);
            i["derivations"][0]["evidenceInputId"] = json!("evidence-input");
        });
        replace_fixture_evidence(&f, br#"{"schemaVersion":"1.0","outputPath":"dist/main.js","inputIds":["code-input","dependency-input"],"toolInputIds":["manager-input","tool-input"]}"#);
        let report = evaluate(&f.repository, &artifact_request(&f)).unwrap();
        assert_eq!((report.files_checked, report.inputs_checked, report.dependencies_checked), (7, 9, 1));
        rewrite_json(&f.artifact, "inventory.json", |i| {
            i["inputs"].as_array_mut().unwrap().retain(|v| v["id"] != "dependency-input");
            i["derivations"][0]["inputIds"] = json!(["code-input"]);
        });
        std::fs::remove_file(f.inputs.join("src/dependency.js")).unwrap();
        replace_fixture_evidence(
            &f,
            br#"{"schemaVersion":"1.0","outputPath":"dist/main.js","inputIds":["code-input"],"toolInputIds":["manager-input","tool-input"]}"#,
        );
        artifact_error(&f, &artifact_request(&f), "PROVENANCE_INCOMPLETE");
    }

    // Removing strict typed deserialization or any scalar/collection cap must fail this test.
    fn strict_objects<T: Metadata + std::fmt::Debug>(bytes: &[u8]) {
        assert!(parse_metadata::<T>(bytes).is_ok());
        let value: Value = serde_json::from_slice(bytes).unwrap();
        fn objects(value: &Value, pointer: String, out: &mut Vec<String>) {
            match value {
                Value::Object(map) => {
                    out.push(pointer.clone());
                    for (key, child) in map {
                        objects(child, format!("{pointer}/{key}"), out);
                    }
                }
                Value::Array(array) => {
                    for (index, child) in array.iter().enumerate() {
                        objects(child, format!("{pointer}/{index}"), out);
                    }
                }
                _ => {}
            }
        }
        let mut pointers = Vec::new();
        objects(&value, String::new(), &mut pointers);
        for pointer in pointers {
            let object = value.pointer(&pointer).unwrap().as_object().unwrap();
            let encoded = serde_json::to_string(object).unwrap();
            let mut injections = vec![format!("\"unexpected\":true,{}", &encoded[1..])];
            injections.extend(object.iter().map(|(key, field)| format!("{}:{},{}", serde_json::to_string(key).unwrap(), field, &encoded[1..])));
            for injected in injections {
                let mutated = serde_json::to_string(&value).unwrap().replacen(&encoded, &format!("{{{injected}"), 1);
                assert!(serde_json::from_str::<T>(&mutated).is_err(), "accepted unknown/duplicate at {pointer}");
            }
            for key in object.keys() {
                let mut omitted = value.clone();
                omitted.pointer_mut(&pointer).unwrap().as_object_mut().unwrap().remove(key);
                assert!(serde_json::from_slice::<T>(&serde_json::to_vec(&omitted).unwrap()).is_err(), "accepted omitted {pointer}/{key}");
            }
        }
        for version in ["1", "1.1", "2.0", "01.0"] {
            let mut changed = value.clone();
            changed["schemaVersion"] = json!(version);
            assert!(parse_metadata::<T>(&serde_json::to_vec(&changed).unwrap()).is_err());
        }
    }

    // A prefix is not yet a directory root; querying it can fail before RootDir is appended.
    #[test]
    fn distribution_directory_roots_wait_for_complete_prefix() {
        for prefix in ["C:", r"\\?\C:", r"\\server\share", r"\\?\UNC\server\share"] {
            let mut checked = PathBuf::new();
            let prefix_result = test_support::directory_step_using(&mut checked, std::ffi::OsStr::new(prefix), true, |_| {
                Err(crate::runtime_artifact::GateError { code: ErrorCode::IoError, subject: None })
            });
            assert!(prefix_result.is_ok(), "a prefix-only step must defer metadata until RootDir: {prefix_result:?}");
            let mut observed = Vec::new();
            test_support::directory_step_using(&mut checked, std::ffi::OsStr::new(std::path::MAIN_SEPARATOR_STR), false, |path| {
                observed.push(path.to_owned());
                Ok(true)
            })
            .unwrap();
            test_support::directory_step_using(&mut checked, std::ffi::OsStr::new("stage"), false, |path| {
                observed.push(path.to_owned());
                Ok(true)
            })
            .unwrap();
            assert_eq!(observed.len(), 2, "the complete root and descendant both still require checks");
            assert!(observed[0].has_root());
            assert_eq!(observed[1], checked);
            assert_eq!(
                test_support::directory_step_using(&mut checked, std::ffi::OsStr::new("linked"), false, |_| Ok(false)).unwrap_err().code,
                ErrorCode::UnsafePath
            );
        }
        // This checks the real host component parser; Windows prefix parsing is tested separately below.
        let f = distribution_fixture();
        let mut visited = Vec::new();
        let checked = test_support::directory_ancestors_using(&f.artifact, |path| {
            visited.push(path.to_owned());
            std::fs::symlink_metadata(path)
                .map(|m| m.is_dir())
                .map_err(|_| crate::runtime_artifact::GateError { code: ErrorCode::IoError, subject: None })
        })
        .unwrap();
        assert_eq!(checked, f.artifact);
        let expected: Vec<_> = f.artifact.ancestors().map(Path::to_owned).collect();
        assert_eq!(visited, expected.into_iter().rev().collect::<Vec<_>>());
        assert_eq!(
            test_support::directory_ancestors_using(&f.artifact.join("../outside"), |path| {
                assert!(!path.ends_with("outside"), "do not query beyond a rejected parent component");
                Ok(true)
            })
            .unwrap_err()
            .code,
            ErrorCode::UnsafePath
        );
    }

    #[cfg(windows)]
    #[test]
    fn distribution_windows_directory_roots_are_complete() {
        // Pure callbacks only: even UNC cases never contact a share or inspect a real remote path.
        for (path, expected) in [
            (r"C:\stage", vec![r"C:\", r"C:\stage"]),
            (r"\\?\C:\stage", vec![r"\\?\C:\", r"\\?\C:\stage"]),
            (r"\\server\share\stage", vec![r"\\server\share\", r"\\server\share\stage"]),
            (r"\\?\UNC\server\share\stage", vec![r"\\?\UNC\server\share\", r"\\?\UNC\server\share\stage"]),
        ] {
            let mut observed = Vec::new();
            let checked = test_support::directory_ancestors_using(Path::new(path), |directory| {
                observed.push(directory.to_owned());
                Ok(true)
            })
            .unwrap();
            assert_eq!(checked, Path::new(path));
            assert_eq!(observed, expected.into_iter().map(PathBuf::from).collect::<Vec<_>>(), "{path}");
        }
    }

    // Following links, normalizing hostile paths, unbounded reads or accepting a changed stage must break this test.
    #[test]
    fn distribution_paths_and_tree_are_closed() {
        let f = distribution_fixture();
        put_bytes(&f.artifact, "z/empty", b"");
        put_bytes(&f.artifact, "abc", b"abc");
        assert_eq!(
            inspect_tree(&f.artifact).unwrap(),
            vec![
                ObservedFile {
                    path: "abc".into(),
                    digest: "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
                    size_bytes: 3,
                },
                ObservedFile {
                    path: "z/empty".into(),
                    digest: "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
                    size_bytes: 0,
                },
            ],
            "tree inspection must return the exact sorted regular-file bytes, not an empty observation"
        );
        let sample = valid_artifact_case();
        let inventory: Value = serde_json::from_slice(&std::fs::read(sample.artifact.join("inventory.json")).unwrap()).unwrap();
        for (path, want) in [
            ("../x", ErrorCode::UnsafePath),
            ("/x", ErrorCode::UnsafePath),
            ("C:/x", ErrorCode::UnsafePath),
            ("a\\b", ErrorCode::UnsafePath),
            ("\\\\server\\x", ErrorCode::UnsafePath),
            ("//server/x", ErrorCode::UnsafePath),
            ("a//b", ErrorCode::UnsafePath),
            ("a/./b", ErrorCode::UnsafePath),
            ("CON", ErrorCode::UnsafePath),
            ("a/nul.txt", ErrorCode::UnsafePath),
            ("COM1.js", ErrorCode::UnsafePath),
            ("lpt9", ErrorCode::UnsafePath),
            ("a.", ErrorCode::UnsafePath),
            ("a ", ErrorCode::UnsafePath),
            ("ee/x", ErrorCode::EnterpriseInput),
            ("a/EE/x", ErrorCode::EnterpriseInput),
        ] {
            let mut changed = inventory.clone();
            changed["outputs"][0]["path"] = json!(path);
            assert_eq!(parse_metadata::<Inventory>(&serde_json::to_vec(&changed).unwrap()).unwrap_err().code, want, "{path}");
        }
        assert_eq!(inspect_tree(&f.root.join("missing")).unwrap_err().code, ErrorCode::IoError);
        assert_eq!(inspect_tree(&f.artifact.join("abc")).unwrap_err().code, ErrorCode::UnsafePath);
        let streamed = distribution_fixture();
        put_bytes(&streamed.artifact, "chunked", &vec![b'a'; 65_537]);
        assert_eq!(
            inspect_tree(&streamed.artifact).unwrap(),
            vec![ObservedFile {
                path: "chunked".into(),
                digest: "sha256:008ffc88d3c96a9f307524eb361e47c5222a887fc45fa0c1fb8d429c5c23b430".into(),
                size_bytes: 65_537,
            }],
            "bytes beyond the fixed hash buffer must also be included"
        );

        // Lowered limits exercise the real collector/hash path without allocating 100,001 files or reading GiB.
        for (entries, file_bytes, total_bytes) in [(2, 10, 10), (10, 2, 10), (10, 10, 2)] {
            let result = test_support::inspect_tree_using(&f.artifact, entries, file_bytes, total_bytes, |_| -> std::io::Result<std::fs::File> {
                panic!("bounds must reject the complete tree before opening payload bytes")
            });
            assert_eq!(result.unwrap_err().code, ErrorCode::LimitExceeded);
        }
        assert_eq!(test_support::inspect_tree_using(&f.artifact, 3, 3, 3, |p| std::fs::File::open(p)).unwrap().len(), 2);
        // Keep the real defaults, but never assume set_len creates sparse files on the host FS.
        assert_eq!(test_support::default_tree_limits(), (100_000, 2_147_483_648, 68_719_476_736));
        let total = distribution_fixture();
        put_bytes(&total.artifact, "one", b"aa");
        put_bytes(&total.artifact, "two", b"bb");
        let over_total = test_support::inspect_tree_using(&total.artifact, 2, 2, 3, |_| -> std::io::Result<std::fs::File> {
            panic!("cumulative budget must reject before either file is opened")
        });
        assert_eq!(over_total.unwrap_err().code, ErrorCode::LimitExceeded);
        assert_eq!(test_support::inspect_tree_using(&total.artifact, 2, 2, 4, |path| std::fs::File::open(path)).unwrap().len(), 2);
        assert_eq!(
            test_support::inspect_tree_using(&f.artifact, 3, 3, 3, |_| Err::<std::fs::File, _>(std::io::Error::other("secret"))).unwrap_err().code,
            ErrorCode::IoError
        );
        assert_eq!(
            test_support::inspect_tree_using(&f.artifact, 3, 3, 3, |_| Ok(std::io::Cursor::new(b""))).unwrap_err().code,
            ErrorCode::DigestMismatch
        );
        for (file_bytes, total_bytes) in [(3, 10), (10, 3)] {
            assert_eq!(
                test_support::inspect_tree_using(&f.artifact, 3, file_bytes, total_bytes, |_| Ok(std::io::Cursor::new(b"abcd"))).unwrap_err().code,
                ErrorCode::LimitExceeded
            );
        }
        struct ReadError;
        impl std::io::Read for ReadError {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("secret read failure"))
            }
        }
        let read_error = test_support::inspect_tree_using(&f.artifact, 3, 3, 3, |_| Ok(ReadError)).unwrap_err();
        assert_eq!(read_error.code, ErrorCode::IoError);
        assert_eq!(read_error.subject.as_deref(), Some("abc"));
        assert!(!format!("{read_error:?}").contains("secret"));

        struct ChangedReader {
            file: std::fs::File,
            modified: std::time::SystemTime,
            changed: bool,
        }
        impl std::io::Read for ChangedReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let count = std::io::Read::read(&mut self.file, buf)?;
                if count == 0 && !self.changed {
                    self.file.set_times(std::fs::FileTimes::new().set_modified(self.modified))?;
                    self.changed = true;
                }
                Ok(count)
            }
        }
        let changed = test_support::inspect_tree_using(&f.artifact, 3, 3, 3, |p| {
            let file = std::fs::OpenOptions::new().read(true).write(true).open(p)?;
            let modified = file.metadata()?.modified()? + std::time::Duration::from_secs(10);
            Ok(ChangedReader { file, modified, changed: false })
        })
        .unwrap_err();
        assert_eq!(changed.code, ErrorCode::DigestMismatch, "same-size metadata change after reading is not a frozen stage");
        assert_eq!(changed.subject.as_deref(), Some("abc"));

        #[cfg(unix)]
        {
            for (path, want) in [
                ("a\\b", ErrorCode::UnsafePath),
                ("NUL.txt", ErrorCode::UnsafePath),
                ("a.", ErrorCode::UnsafePath),
                ("a ", ErrorCode::UnsafePath),
                ("ee/x", ErrorCode::EnterpriseInput),
                ("EE/x", ErrorCode::EnterpriseInput),
            ] {
                let bad = distribution_fixture();
                put_bytes(&bad.artifact, path, b"original forbidden-path fixture");
                assert_eq!(inspect_tree(&bad.artifact).unwrap_err().code, want, "{path}");
            }
            let linked = distribution_fixture();
            put_bytes(&linked.inputs, "outside", b"original outside bytes must never be opened");
            std::fs::create_dir(linked.inputs.join("child")).unwrap();
            for (destination, target) in [("file-link", linked.inputs.join("outside")), ("dir-link", linked.inputs.clone())] {
                std::os::unix::fs::symlink(target, linked.artifact.join(destination)).unwrap();
                assert_eq!(
                    test_support::inspect_tree_using(&linked.artifact, 10, 100, 100, |_| -> std::io::Result<std::fs::File> {
                        panic!("symlink escaped into outside bytes")
                    })
                    .unwrap_err()
                    .code,
                    ErrorCode::UnsafePath
                );
                std::fs::remove_file(linked.artifact.join(destination)).unwrap();
            }
            let root_link = linked.root.join("root-link");
            std::os::unix::fs::symlink(&linked.inputs, &root_link).unwrap();
            for root in [&root_link, &root_link.join("child")] {
                assert_eq!(inspect_tree(root).unwrap_err().code, ErrorCode::UnsafePath, "linked root or ancestor");
            }
            let _socket = std::os::unix::net::UnixListener::bind(linked.artifact.join("socket")).unwrap();
            assert_eq!(inspect_tree(&linked.artifact).unwrap_err().code, ErrorCode::UnsafePath);
            let directory_change = distribution_fixture();
            put_bytes(&directory_change.artifact, "original", b"abc");
            let modified = std::fs::metadata(&directory_change.artifact).unwrap().modified().unwrap() + std::time::Duration::from_secs(10);
            let changed = test_support::inspect_tree_using(&directory_change.artifact, 1, 3, 3, |path| {
                std::fs::File::open(&directory_change.artifact)?.set_times(std::fs::FileTimes::new().set_modified(modified))?;
                std::fs::File::open(path)
            })
            .unwrap_err();
            assert_eq!(changed.code, ErrorCode::DigestMismatch, "directory metadata must also remain stable");
        }
        #[cfg(target_os = "linux")]
        {
            let alias = distribution_fixture();
            put_bytes(&alias.artifact, "Case/one", b"a");
            put_bytes(&alias.artifact, "case/two", b"b");
            assert_eq!(inspect_tree(&alias.artifact).unwrap_err().code, ErrorCode::UnsafePath, "directory aliases also collide");
        }
    }

    fn source_only_case() -> DistributionFixture {
        let f = distribution_fixture();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        for path in ["packaging/mastra/policy.json", "packaging/mastra/source-records.json"] {
            put_bytes(&f.repository, path, &std::fs::read(root.join(path)).unwrap());
        }
        put_bytes(&f.repository, "packages/mastra-runtime/package.json", b"{\"private\":true}\n");
        put_bytes(&f.repository, "packages/mastra-runtime/src/main.ts", b"// Original source-only fixture; never executed.\n");
        f
    }

    fn host_only_request(root: &Path) -> Request {
        Request {
            mode: Mode::HostOnly,
            tree: Some(root.to_owned()),
            tree_kind: Some(TreeKind::Repository),
            target: Some("x86_64-unknown-linux-gnu".into()),
            expected_revision: Some("1234567890abcdef1234567890abcdef12345678".into()),
            advertised: Some(false),
            inputs_root: None,
            approval_id: None,
            expected_contract_digest: None,
            expected_policy_digest: None,
            expected_sources_digest: None,
        }
    }

    // Advertising availability, hiding named payloads or substituting another tree must never yield a package pass.
    #[test]
    fn distribution_host_only_cannot_hide_payload_or_availability() {
        let f = source_only_case();
        let request = host_only_request(&f.repository);
        let result = evaluate(&f.repository, &request);
        assert!(result.is_ok(), "explicit source-only repository must be inspectable: {result:?}");
        assert_eq!(evaluate(&f.repository, &request).unwrap().status, GateStatus::NotApplicable);
        assert_eq!(evaluate(&f.repository, &request).unwrap().inventory_digest, None);
        let mut relative = request.clone();
        relative.tree = Some(PathBuf::from("."));
        assert_eq!(evaluate(&f.repository, &relative).unwrap().status, GateStatus::NotApplicable);
        for path in [
            ".git/runtime.json",
            "target/node",
            "target-cache/runtime.json",
            "node_modules/mastra/node.exe",
            "packages/mastra-runtime/node_modules/dep/runtime.json",
        ] {
            put_bytes(&f.repository, path, b"original inert cache fixture, not runtime coverage");
        }
        let report = evaluate(&f.repository, &request).unwrap();
        assert_eq!(report.mode, Mode::HostOnly);
        assert_eq!(report.status, GateStatus::NotApplicable);
        assert_eq!(report.tree_kind, Some(TreeKind::Repository));
        assert_eq!(report.inventory_digest, None);
        assert_eq!((report.files_checked, report.inputs_checked, report.dependencies_checked), (4, 0, 0));
        assert_eq!(report.policy_digest, digest(&std::fs::read(f.repository.join("packaging/mastra/policy.json")).unwrap()));
        assert_eq!(report.source_records_digest, digest(&std::fs::read(f.repository.join("packaging/mastra/source-records.json")).unwrap()));
        assert_eq!(serde_json::to_value(&report).unwrap()["schemaVersion"], "1.0");
        assert_eq!(inspect_tree(&f.repository).unwrap().len(), 9, "strict artifact/input tree inspection has NO repository exclusions");
        let mut unsupported = request.clone();
        unsupported.target = Some("wasm32-unknown-unknown".into());
        let report = evaluate(&f.repository, &unsupported).unwrap();
        assert_eq!(report.target.as_deref(), Some("wasm32-unknown-unknown"));
        assert_eq!(report.status, GateStatus::NotApplicable);
        unsupported.advertised = Some(true);
        assert_eq!(evaluate(&f.repository, &unsupported).unwrap_err().code, ErrorCode::InvalidArgument);

        for field in [
            "mode-policy",
            "mode-artifact",
            "tree",
            "tree-kind",
            "package",
            "other-tree",
            "target",
            "bad-target",
            "revision",
            "bad-revision",
            "advertised",
            "available",
            "inputs",
            "approval",
            "contract",
            "policy",
            "sources",
        ] {
            let mut bad = request.clone();
            match field {
                "mode-policy" => bad.mode = Mode::Policy,
                "mode-artifact" => bad.mode = Mode::Artifact,
                "tree" => bad.tree = None,
                "tree-kind" => bad.tree_kind = None,
                "package" => bad.tree_kind = Some(TreeKind::Package),
                "other-tree" => bad.tree = Some(f.artifact.clone()),
                "target" => bad.target = None,
                "bad-target" => bad.target = Some("../invalid".into()),
                "revision" => bad.expected_revision = None,
                "bad-revision" => bad.expected_revision = Some("main".into()),
                "advertised" => bad.advertised = None,
                "available" => bad.advertised = Some(true),
                "inputs" => bad.inputs_root = Some(f.inputs.clone()),
                "approval" => bad.approval_id = Some("fixture-approval".into()),
                "contract" => bad.expected_contract_digest = Some(digest(b"original")),
                "policy" => bad.expected_policy_digest = Some(digest(b"original")),
                "sources" => bad.expected_sources_digest = Some(digest(b"original")),
                _ => unreachable!(),
            }
            assert_eq!(evaluate(&f.repository, &bad).unwrap_err().code, ErrorCode::InvalidArgument, "{field}");
        }
        for path in [
            "runtime.json",
            "dist/runtime.json",
            "dist/node",
            "node.exe",
            "bin/Node.EXE",
            "nested/target-cache/node",
            "packages/mastra-runtime/dist/main.js",
            "resources/mastra/main.js",
            "packaging/mastra/nested/mastra/main.js",
            "nested/packaging/mastra/main.js",
        ] {
            let polluted = source_only_case();
            let request = host_only_request(&polluted.repository);
            put_bytes(&polluted.repository, path, b"original inert named-payload fixture");
            assert!(evaluate(&polluted.repository, &request).is_err());
            assert_eq!(evaluate(&polluted.repository, &request).unwrap_err().code, ErrorCode::RuntimePresent, "{path}");
        }
        for path in ["mastra", "resources/mastra", "packages/mastra-runtime/dist"] {
            let staged = source_only_case();
            std::fs::create_dir_all(staged.repository.join(path)).unwrap();
            assert_eq!(
                evaluate(&staged.repository, &host_only_request(&staged.repository)).unwrap_err().code,
                ErrorCode::RuntimePresent,
                "empty {path}"
            );
        }
        put_bytes(&f.repository, "runtime.json", b"original inert receipt marker");
        put_bytes(&f.repository, "dist/node", b"original inert surviving payload");
        std::fs::remove_file(f.repository.join("runtime.json")).unwrap();
        assert_eq!(evaluate(&f.repository, &request).unwrap_err().code, ErrorCode::RuntimePresent, "deleting a receipt does not remove its payload");
        unsupported.advertised = Some(false);
        assert_eq!(
            evaluate(&f.repository, &unsupported).unwrap_err().code,
            ErrorCode::RuntimePresent,
            "unsupported target is not a payload exemption"
        );
    }

    // A scoped organization grant must never promote unaudited per-file rights.
    #[test]
    fn distribution_scoped_grant_is_not_per_file_verification() {
        let f = valid_artifact_case();
        let sources: Value = serde_json::from_slice(&std::fs::read(f.repository.join("packaging/mastra/source-records.json")).unwrap()).unwrap();
        assert!(parse_metadata::<SourceRecords>(&serde_json::to_vec(&sources).unwrap()).is_ok());
        let mut promoted = sources.clone();
        promoted["records"][0]["verification"]["status"] = json!("verified");
        assert!(
            parse_metadata::<SourceRecords>(&serde_json::to_vec(&promoted).unwrap()).is_err(),
            "scoped grant is not per-file verification: missing rights and independent third-party audit must reject verified"
        );

        let repository = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        check_repository_policy(repository).unwrap();
        let policy = read_policy(repository).unwrap();
        let real_sources = read_source_records(repository).unwrap();
        let policy_value: Value = serde_json::from_slice(&std::fs::read(repository.join("packaging/mastra/policy.json")).unwrap()).unwrap();
        let real_value: Value = serde_json::from_slice(&std::fs::read(repository.join("packaging/mastra/source-records.json")).unwrap()).unwrap();
        assert_eq!(policy_value["approvals"], json!([]), "no real artifact has been approved");
        assert_eq!(policy_value["policyId"], "solidcraft-desktop-mastra-v1");
        assert_eq!(
            policy_value["supportedTargets"],
            json!(["aarch64-apple-darwin", "x86_64-apple-darwin", "x86_64-pc-windows-msvc", "x86_64-unknown-linux-gnu"])
        );
        assert_eq!(policy_value["allowedLicenseExpressions"], json!(["Apache-2.0", "MIT", "MIT OR Apache-2.0"]));
        assert_eq!(real_value["authorizations"][0]["declaration"], "有权代表并授权");
        assert_eq!(real_value["authorizations"][0]["sourceRevision"], "acb5048120b53ae1b4502724b70d1283e8c9daf4");
        let records = real_value["records"].as_array().unwrap();
        assert_eq!(records.len(), 5);
        for (record, (id, role, path)) in records.iter().zip([
            ("agent-loop", "agent-loop", "apps/server/src/mastra/agent-loop/agent-loop.ts"),
            ("cad-generation", "orchestration", "packages/ai-runtime/src/workflows/cad-generation.ts"),
            ("model-runtime", "model-runtime", "packages/ai-runtime/src/models/model-runtime.ts"),
            ("product-generation-runtime", "orchestration", "apps/server/src/mastra/services/product-generation-runtime.ts"),
            ("query-proposal", "orchestration", "apps/server/src/mastra/workflows/query-proposal.ts"),
        ]) {
            assert_eq!(record["id"], id);
            assert_eq!(record["role"], role);
            assert_eq!(record["sourcePath"], path);
            for field in ["sourceDigest", "destinationPath", "destinationDigest", "adaptation", "rightsHolder", "licenseExpression"] {
                assert!(record[field].is_null(), "candidate {id} has invented {field}");
            }
            assert_eq!(record["requiredNotices"], json!([]));
            assert_eq!(record["dependencyIds"], json!([]));
            for review in ["thirdPartyReview", "verification"] {
                assert_eq!(record[review], json!({"status":"unverified", "evidence":[]}));
            }
        }

        let inventory_value: Value = serde_json::from_slice(&std::fs::read(f.artifact.join("inventory.json")).unwrap()).unwrap();
        parse_metadata::<Inventory>(&serde_json::to_vec(&inventory_value).unwrap()).unwrap().validate_sources(&policy, &real_sources).unwrap();
        let mut used = inventory_value.clone();
        used["inputs"][0]["sourceRevision"] = json!("acb5048120b53ae1b4502724b70d1283e8c9daf4");
        for id in ["agent-loop", "cad-generation", "model-runtime", "product-generation-runtime", "query-proposal"] {
            used["inputs"][0]["sourceRecordId"] = json!(id);
            let error =
                parse_metadata::<Inventory>(&serde_json::to_vec(&used).unwrap()).unwrap().validate_sources(&policy, &real_sources).unwrap_err();
            assert_eq!(error.code, ErrorCode::SourceRightsUnverified);
            assert_eq!(error.subject.as_deref(), Some(id));
        }
        used["inputs"][0]["sourceRecordId"] = json!("missing-source");
        assert_eq!(
            parse_metadata::<Inventory>(&serde_json::to_vec(&used).unwrap()).unwrap().validate_sources(&policy, &real_sources).unwrap_err().code,
            ErrorCode::SourceRightsUnverified
        );

        for (pointer, value, want) in [
            ("/records/0/authorizationId", json!("missing-grant"), ErrorCode::SourceRightsUnverified),
            ("/records/0/sourceRepository", json!("https://fixture.invalid/other"), ErrorCode::SourceRightsUnverified),
            ("/records/0/sourceRevision", json!("0000000000000000000000000000000000000000"), ErrorCode::SourceRightsUnverified),
            ("/authorizations/0/eligibleRoles", json!(["agent-loop"]), ErrorCode::SourceRightsUnverified),
            ("/authorizations/0/excludedRoles", json!([]), ErrorCode::SourceRightsUnverified),
            ("/authorizations/0/eligibleRoles", json!(["account"]), ErrorCode::InvalidMetadata),
            ("/authorizations/0/licenseGrant", json!("MIT"), ErrorCode::InvalidMetadata),
            ("/records/0/sourcePath", json!("src/*"), ErrorCode::UnsafePath),
            ("/records/0/destinationPath", json!("src/*"), ErrorCode::UnsafePath),
            ("/records/0/sourcePath", json!("src/EE/original.ts"), ErrorCode::EnterpriseInput),
        ] {
            let mut changed = sources.clone();
            *changed.pointer_mut(pointer).unwrap() = value;
            assert_eq!(parse_metadata::<SourceRecords>(&serde_json::to_vec(&changed).unwrap()).unwrap_err().code, want, "accepted {pointer}");
        }
        // Even editing both sides of a reference cannot broaden the relayed real grant.
        for (pointer, value) in [
            ("/authorizations/0/id", json!("fabricated-grant")),
            ("/authorizations/0/actor", json!("Invented named signatory")),
            ("/authorizations/0/declaration", json!("All source is original")),
            ("/authorizations/0/proposition", json!("Includes account/database/server-bootstrap/script-execution code")),
            ("/authorizations/0/sourceRepository", json!("https://fixture.invalid/other")),
            ("/authorizations/0/sourceRevision", json!("0000000000000000000000000000000000000000")),
            ("/authorizations/0/eligibleRoles", json!(["agent-loop"])),
            ("/authorizations/0/excludedRoles", json!([])),
            ("/authorizations/0/evidence", json!("Fabricated public attestation")),
        ] {
            let mut changed = real_value.clone();
            match pointer {
                "/authorizations/0/sourceRepository" => {
                    for record in changed["records"].as_array_mut().unwrap() {
                        record["sourceRepository"] = value.clone();
                    }
                }
                "/authorizations/0/sourceRevision" => {
                    for record in changed["records"].as_array_mut().unwrap() {
                        record["sourceRevision"] = value.clone();
                    }
                }
                "/authorizations/0/id" => {
                    for record in changed["records"].as_array_mut().unwrap() {
                        record["authorizationId"] = value.clone();
                    }
                }
                _ => {}
            }
            *changed.pointer_mut(pointer).unwrap() = value;
            assert_eq!(
                parse_metadata::<SourceRecords>(&serde_json::to_vec(&changed).unwrap()).unwrap_err().code,
                ErrorCode::PolicyMismatch,
                "broadened known grant via {pointer}"
            );
        }

        let upstream = b"Original synthetic upstream module.\n";
        let adapted = b"Original synthetic adapted module.\n";
        let notice = b"Original synthetic retained third-party notice.\n";
        put_bytes(&f.repository, "src/adapted.ts", adapted);
        put_bytes(&f.repository, "licenses/source-notice.txt", notice);
        let mut verified = sources.clone();
        let record = &mut verified["records"][0];
        record["sourceDigest"] = json!(digest(upstream));
        record["destinationPath"] = json!("src/adapted.ts");
        record["destinationDigest"] = json!(digest(adapted));
        record["adaptation"] = json!("Original synthetic adaptation, not imported code");
        record["rightsHolder"] = json!("intgral-ai");
        record["licenseExpression"] = json!("MIT");
        record["requiredNotices"] = json!([{"path":"licenses/source-notice.txt", "digest":digest(notice)}]);
        record["dependencyIds"] = json!(["fixture-dependency"]);
        record["thirdPartyReview"] =
            json!({"status":"verified", "evidence":["Synthetic independent audit: fixture dependency has its own MIT license and retained notice"]});
        record["verification"] = json!({"status":"verified", "evidence":["Synthetic per-file scope, ownership, license and adaptation audit"]});
        put_bytes(&f.repository, "packaging/mastra/source-records.json", &serde_json::to_vec(&verified).unwrap());
        check_repository_policy(&f.repository).unwrap();
        let synthetic_policy = read_policy(&f.repository).unwrap();
        let synthetic_sources = read_source_records(&f.repository).unwrap();
        let mut licensed_use = inventory_value.clone();
        licensed_use["inputs"][0]["sourceRecordId"] = json!("fixture-source");
        licensed_use["inputs"][0]["sourceRevision"] = verified["records"][0]["sourceRevision"].clone();
        licensed_use["licenses"][0]["rightsHolder"] = json!("intgral-ai");
        licensed_use["licenses"][0]["noticeFiles"] = verified["records"][0]["requiredNotices"].clone();
        parse_metadata::<Inventory>(&serde_json::to_vec(&licensed_use).unwrap())
            .unwrap()
            .validate_sources(&synthetic_policy, &synthetic_sources)
            .unwrap();

        for (pointer, value) in [
            ("/records/0/sourceDigest", Value::Null),
            ("/records/0/destinationPath", Value::Null),
            ("/records/0/destinationDigest", Value::Null),
            ("/records/0/adaptation", Value::Null),
            ("/records/0/rightsHolder", Value::Null),
            ("/records/0/rightsHolder", json!("Unverified third-party owner")),
            ("/records/0/licenseExpression", Value::Null),
            ("/records/0/thirdPartyReview/status", json!("unverified")),
            ("/records/0/thirdPartyReview/evidence", json!([])),
            ("/records/0/verification/evidence", json!([])),
            ("/records/0/role", json!("account")),
            ("/records/0/sourceRepository", json!("https://fixture.invalid/other")),
            ("/records/0/sourceRevision", json!("0000000000000000000000000000000000000000")),
            ("/authorizations/0/eligibleRoles", json!(["agent-loop"])),
        ] {
            let mut changed = verified.clone();
            *changed.pointer_mut(pointer).unwrap() = value;
            assert!(parse_metadata::<SourceRecords>(&serde_json::to_vec(&changed).unwrap()).is_err(), "verified accepted {pointer}");
        }
        for expression in ["GPL-3.0-only", "LGPL-2.1-only", "AGPL-3.0-only", "Unknown", "MIT OR GPL-3.0-only"] {
            let mut changed = verified.clone();
            changed["records"][0]["licenseExpression"] = json!(expression);
            put_bytes(&f.repository, "packaging/mastra/source-records.json", &serde_json::to_vec(&changed).unwrap());
            assert_eq!(check_repository_policy(&f.repository).unwrap_err().code, ErrorCode::SourceRightsUnverified);
        }
        for (pointer, value) in [
            ("/inputs/0/sourceRevision", Value::Null),
            ("/inputs/0/sourceRevision", json!("0000000000000000000000000000000000000000")),
            ("/dependencies", json!([])),
            ("/licenses/0/rightsHolder", json!("Some other owner")),
            ("/licenses/0/expression", json!("Apache-2.0")),
            ("/licenses/0/noticeFiles", json!([])),
        ] {
            let mut changed = licensed_use.clone();
            *changed.pointer_mut(pointer).unwrap() = value;
            assert_eq!(
                parse_metadata::<Inventory>(&serde_json::to_vec(&changed).unwrap())
                    .unwrap()
                    .validate_sources(&synthetic_policy, &synthetic_sources)
                    .unwrap_err()
                    .code,
                ErrorCode::SourceRightsUnverified,
                "used source lost its own evidence at {pointer}"
            );
        }
        rewrite_json(&f.repository, "packaging/mastra/policy.json", |p| {
            p["supportedTargets"] = json!(["aarch64-apple-darwin"]);
        });
        assert_eq!(read_policy(&f.repository).unwrap_err().code, ErrorCode::PolicyMismatch);
    }

    #[test]
    fn distribution_metadata_is_strict_and_bounded() {
        let f = valid_artifact_case();
        let policy_bytes = std::fs::read(f.repository.join("packaging/mastra/policy.json")).unwrap();
        let source_bytes = std::fs::read(f.repository.join("packaging/mastra/source-records.json")).unwrap();
        let inventory_bytes = std::fs::read(f.artifact.join("inventory.json")).unwrap();
        let receipt_bytes = std::fs::read(f.artifact.join("runtime.json")).unwrap();
        strict_objects::<Policy>(&policy_bytes);
        strict_objects::<SourceRecords>(&source_bytes);
        strict_objects::<Inventory>(&inventory_bytes);
        strict_objects::<RuntimeReceipt>(&receipt_bytes);
        let policy = parse_metadata::<Policy>(&policy_bytes).unwrap();
        parse_metadata::<Inventory>(&inventory_bytes).unwrap().validate_licenses(&policy).unwrap();
        let inventory: Value = serde_json::from_slice(&inventory_bytes).unwrap();
        let sources: Value = serde_json::from_slice(&source_bytes).unwrap();
        let mut populated = sources.clone();
        for (field, value) in [
            ("sourceDigest", json!(digest(b"upstream fixture"))),
            ("destinationPath", json!("src/adapted.ts")),
            ("destinationDigest", json!(digest(b"adapted fixture"))),
            ("adaptation", json!("Synthetic adaptation")),
            ("rightsHolder", json!("Synthetic owner")),
            ("licenseExpression", json!("MIT")),
        ] {
            populated["records"][0][field] = value;
        }
        assert!(parse_metadata::<SourceRecords>(&serde_json::to_vec(&populated).unwrap()).is_ok());
        for field in ["authorizations", "records"] {
            let mut changed = sources.clone();
            let duplicate = changed[field][0].clone();
            changed[field].as_array_mut().unwrap().push(duplicate);
            assert!(parse_metadata::<SourceRecords>(&serde_json::to_vec(&changed).unwrap()).is_err());
        }
        for pointer in [
            "/records/0/sourceDigest",
            "/records/0/destinationPath",
            "/records/0/destinationDigest",
            "/records/0/adaptation",
            "/records/0/rightsHolder",
            "/records/0/licenseExpression",
        ] {
            let mut changed = sources.clone();
            changed["records"][0].as_object_mut().unwrap().remove(pointer.rsplit('/').next().unwrap());
            assert!(serde_json::from_value::<SourceRecords>(changed).is_err(), "accepted omitted {pointer}");
        }
        for (container, field) in [("inputs", "sourceRevision"), ("inputs", "sourceRecordId"), ("derivations", "evidenceInputId")] {
            let mut changed = inventory.clone();
            changed[container][0].as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<Inventory>(changed).is_err(), "accepted omitted {field}");
        }
        let mut cases: Vec<(&str, Value)> = Vec::new();
        for bad in ["sha256:ab", &format!("sha256:{}", "A".repeat(64)), &format!("sha256:{}", "a".repeat(65))] {
            cases.push(("/node/archiveDigest", json!(bad)));
        }
        for bad in ["^1.2.3", "1.2", "v1.2.3", "01.2.3", "1.02.3", "1.2.03", "1.2.3-01", "1.2.3-", "1.2.3+a", "latest"] {
            cases.push(("/runtimeVersion", json!(bad)));
        }
        for bad in ["ABCDEF1234567890abcdef1234567890abcdef1234", "main", "123", "g".repeat(40).as_str()] {
            cases.push(("/inputs/0/sourceRevision", json!(bad)));
        }
        for bad in [
            "/root",
            "C:/x",
            "//server/x",
            "a\\b",
            "a/../b",
            "a/./b",
            "a//b",
            "a/",
            "a/NUL.txt",
            "com1",
            "LPT9.x",
            "a.",
            "a ",
            "a:b",
            "a?b",
            "a\u{7f}",
            "é",
            "a/EE/b",
            "",
            "a\0b",
            "a<b",
            "a>b",
            "a\"b",
            "a|b",
            "a*b",
        ] {
            cases.push(("/outputs/0/path", json!(bad)));
        }
        for bad in ["", "BAD", "a b", &"a".repeat(129)] {
            cases.push(("/artifactId", json!(bad)));
        }
        for bad in ["http://fixture.invalid/a", "https://", "https://user@fixture.invalid/a", "https://fixture.invalid/a b"] {
            cases.push(("/node/sourceUrl", json!(bad)));
        }
        cases.extend([
            ("/protocolVersions", json!([])),
            ("/protocolVersions", json!(["1.0", "1.0"])),
            ("/target", json!("other")),
            ("/outputs/0/sizeBytes", json!(2_147_483_649_u64)),
            ("/outputs/0/sizeBytes", json!(-1)),
            ("/outputs/0/sizeBytes", json!(1.5)),
            ("/rootDependencyIds", json!(["z", "a"])),
            ("/rootDependencyIds", json!(["a", "a"])),
        ]);
        for (pointer, bad) in cases {
            let mut changed = inventory.clone();
            *changed.pointer_mut(pointer).unwrap() = bad;
            assert!(parse_metadata::<Inventory>(&serde_json::to_vec(&changed).unwrap()).is_err(), "accepted {pointer}: {changed}");
        }
        for path in ["a/ee/b", &"a".repeat(1025)] {
            let mut changed = inventory.clone();
            changed["inputs"][0]["originalPath"] = json!(path);
            assert!(parse_metadata::<Inventory>(&serde_json::to_vec(&changed).unwrap()).is_err());
        }
        for (pointer, good) in [
            ("/artifactId", json!("a".repeat(128))),
            ("/node/executablePath", json!("a".repeat(1024))),
            ("/outputs/0/sizeBytes", json!(2_147_483_648_u64)),
            ("/runtimeVersion", json!("0.0.0-alpha.0-x.1")),
            ("/inputs/0/sourceRecordId", json!("fixture-source")),
        ] {
            let mut changed = inventory.clone();
            *changed.pointer_mut(pointer).unwrap() = good;
            assert!(parse_metadata::<Inventory>(&serde_json::to_vec(&changed).unwrap()).is_ok(), "rejected {pointer} boundary");
        }
        for field in ["dependencies", "licenses", "inputs", "outputs", "derivations"] {
            let mut changed = inventory.clone();
            let first = changed[field][0].clone();
            changed[field].as_array_mut().unwrap().push(first);
            assert!(parse_metadata::<Inventory>(&serde_json::to_vec(&changed).unwrap()).is_err(), "accepted duplicate {field}");
        }
        let mut alias = inventory.clone();
        alias["outputs"][0]["path"] = json!("DIST/MAIN.JS");
        alias["outputs"][1]["path"] = json!("dist/main.js");
        assert!(parse_metadata::<Inventory>(&serde_json::to_vec(&alias).unwrap()).is_err());
        std::fs::remove_file(f.artifact.join("dist/main.js")).unwrap();
        std::fs::remove_file(f.inputs.join("dist/main.js")).unwrap();
        for expression in ["GPL-3.0-only", "LGPL-2.1-only", "AGPL-3.0-only", "Unknown", "MIT OR GPL-3.0-only"] {
            let mut changed = inventory.clone();
            changed["licenses"][0]["expression"] = json!(expression);
            put_bytes(&f.artifact, "inventory.json", &serde_json::to_vec(&changed).unwrap());
            let observed = read_metadata::<Inventory>(&f.artifact, "inventory.json").unwrap();
            // No artifact/input bytes are needed or read for admission of license metadata.
            assert_eq!(observed.validate_licenses(&policy).unwrap_err().code, ErrorCode::SourceRightsUnverified);
        }
        let before = std::fs::read(f.repository.join("packaging/mastra/policy.json")).unwrap();
        rewrite_json(&f.artifact, "inventory.json", |i| i["artifactId"] = json!("changed"));
        assert_eq!(before, std::fs::read(f.repository.join("packaging/mastra/policy.json")).unwrap());
        let mut large = vec![b' '; 16 * 1024 * 1024 + 1];
        assert_eq!(parse_metadata::<Policy>(&large).unwrap_err().code, ErrorCode::LimitExceeded);
        put_bytes(&f.repository, "large.json", &large);
        assert_eq!(read_metadata::<Policy>(&f.repository, "large.json").unwrap_err().code, ErrorCode::LimitExceeded);
        large.clear();
        large.extend_from_slice(&policy_bytes);
        large.resize(16 * 1024 * 1024, b' ');
        assert!(parse_metadata::<Policy>(&large).is_ok());
        put_bytes(&f.repository, "exact.json", &large);
        assert!(read_metadata::<Policy>(&f.repository, "exact.json").is_ok());
        let mut too_many = inventory.clone();
        too_many["rootDependencyIds"] = json!(vec!["a"; 100_001]);
        assert_eq!(parse_metadata::<Inventory>(&serde_json::to_vec(&too_many).unwrap()).unwrap_err().code, ErrorCode::LimitExceeded);
        too_many["rootDependencyIds"] = json!((0..100_000).map(|n| format!("id-{n:06}")).collect::<Vec<_>>());
        assert!(parse_metadata::<Inventory>(&serde_json::to_vec(&too_many).unwrap()).is_ok());
        let mut total = inventory.clone();
        total["inputs"][0]["sizeBytes"] = json!(2_147_483_647_u64);
        total["inputs"][1]["sizeBytes"] = json!(1);
        total["inputs"][2]["sizeBytes"] = json!(1);
        total["outputs"] = json!(
            (0..31)
                .map(|n| {
                    let mut o = inventory["outputs"][0].clone();
                    o["path"] = json!(format!("file-{n:02}"));
                    o["sizeBytes"] = json!(if n == 30 { 2_147_483_647_u64 } else { 2_147_483_648_u64 });
                    o
                })
                .collect::<Vec<_>>()
        );
        // Inputs: 2 GiB + 1; outputs: 62 GiB - 1. Every file is <= 2 GiB.
        assert!(parse_metadata::<Inventory>(&serde_json::to_vec(&total).unwrap()).is_ok(), "combined 64 GiB must be accepted");
        total["inputs"][0]["sizeBytes"] = json!(2_147_483_648_u64);
        // Only the combined total exceeds its cap, now by exactly one byte.
        assert_eq!(parse_metadata::<Inventory>(&serde_json::to_vec(&total).unwrap()).unwrap_err().code, ErrorCode::LimitExceeded);
        let secret = b"{\"schemaVersion\":\"SECRET-not-for-diagnostics\"}";
        let error = parse_metadata::<Policy>(secret).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidMetadata);
        assert!(!format!("{error:?}").contains("SECRET"));
        assert!(read_metadata::<Policy>(&f.repository, "../secret").is_err());
        let missing = read_metadata::<Policy>(&f.repository, "missing.json").unwrap_err();
        assert_eq!(missing.code, ErrorCode::IoError);
        assert_eq!(missing.subject.as_deref(), Some("missing.json"));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&f.artifact, f.repository.join("metadata-link")).unwrap();
            assert_eq!(read_metadata::<RuntimeReceipt>(&f.repository, "metadata-link/runtime.json").unwrap_err().code, ErrorCode::UnsafePath);
        }
    }
}
