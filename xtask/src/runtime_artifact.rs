//! Distribution policy and exact-byte artifact admission; not a complete build-history verifier.
//! Callers must freeze their trees; metadata checks are NOT concurrent-filesystem isolation.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::de::{DeserializeOwned, Error as _, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest as _, Sha256};

const MAX_JSON_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ENTRIES: usize = 100_000;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const POLICY_PATH: &str = "packaging/mastra/policy.json";
const SOURCES_PATH: &str = "packaging/mastra/source-records.json";
const OWNER_GRANT_ID: &str = "vector-studio-owner-declaration";
const SOURCE_REPOSITORY: &str = "https://github.com/intgral-ai/intgral-vector-studio";
const SOURCE_REVISION: &str = "acb5048120b53ae1b4502724b70d1283e8c9daf4";
const OWNER_PROPOSITION: &str = "The owner has the right to represent intgral-ai and authorizes adapting and redistributing in the PUBLIC intgral-ai/open-skyfall-desktop repository, under MIT OR Apache-2.0, intgral-ai-owned Vector Studio orchestration/agent-loop/model-runtime modules selected in contract section 1.5 at acb5048120b53ae1b4502724b70d1283e8c9daf4. This excludes contract-excluded account/database/server-bootstrap/script-execution code. Third-party code keeps its applicable licenses and requires independent audit; this does not attest that every file is original or authorize arbitrary source.";
const OWNER_EVIDENCE: &str = "intgral-ai is the rights holder and authorizing entity. It authorized public distribution of the adapted Vector Studio source files on 2026-11-07, under this per-file rights record and not a repository-wide license: no root license was identified in the private source at acb5048120b53ae1b4502724b70d1283e8c9daf4. Imported files retain their applicable notices; each file's license, notices and transitive dependencies are recorded by the per-file audit, with transitive dependencies inventoried when #7 supplies the artifact. This authorization covers neither npm/Node packages nor full runtime coverage. No public permalink or named personal signatory was supplied.";

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum ErrorCode {
    InvalidArgument,
    InvalidMetadata,
    LimitExceeded,
    PolicyMismatch,
    SourceRightsUnverified,
    UnapprovedArtifact,
    UnsupportedTarget,
    EnterpriseInput,
    UnsafePath,
    DigestMismatch,
    IdentityMismatch,
    FileSetMismatch,
    ProvenanceIncomplete,
    RuntimePresent,
    IoError,
}

impl ErrorCode {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::InvalidArgument => "INVALID_ARGUMENT",
            Self::InvalidMetadata => "INVALID_METADATA",
            Self::LimitExceeded => "LIMIT_EXCEEDED",
            Self::PolicyMismatch => "POLICY_MISMATCH",
            Self::SourceRightsUnverified => "SOURCE_RIGHTS_UNVERIFIED",
            Self::UnapprovedArtifact => "UNAPPROVED_ARTIFACT",
            Self::UnsupportedTarget => "UNSUPPORTED_TARGET",
            Self::EnterpriseInput => "ENTERPRISE_INPUT",
            Self::UnsafePath => "UNSAFE_PATH",
            Self::FileSetMismatch => "FILE_SET_MISMATCH",
            Self::DigestMismatch => "DIGEST_MISMATCH",
            Self::IdentityMismatch => "IDENTITY_MISMATCH",
            Self::ProvenanceIncomplete => "PROVENANCE_INCOMPLETE",
            Self::RuntimePresent => "RUNTIME_PRESENT",
            Self::IoError => "IO_ERROR",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct GateError {
    pub code: ErrorCode,
    pub subject: Option<String>,
}

fn error(code: ErrorCode) -> GateError {
    GateError { code, subject: None }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Mode {
    Policy,
    HostOnly,
    Artifact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TreeKind {
    Repository,
    Package,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GateStatus {
    PolicyValid,
    NotApplicable,
    Verified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    pub mode: Mode,
    pub tree: Option<std::path::PathBuf>,
    pub tree_kind: Option<TreeKind>,
    pub inputs_root: Option<std::path::PathBuf>,
    pub approval_id: Option<String>,
    pub target: Option<String>,
    pub expected_revision: Option<String>,
    pub expected_contract_digest: Option<String>,
    pub expected_policy_digest: Option<String>,
    pub expected_sources_digest: Option<String>,
    pub advertised: Option<bool>,
}

fn cli_mode(value: &str) -> Option<Mode> {
    match value {
        "policy" => Some(Mode::Policy),
        "host-only" => Some(Mode::HostOnly),
        "artifact" => Some(Mode::Artifact),
        _ => None,
    }
}

// Retain an identifiable mode on CLI failure, but never choose between repeated mode flags.
fn identifiable_mode(args: &[&str]) -> Option<Mode> {
    let mut occurrences = args.iter().enumerate().filter(|(_, arg)| **arg == "--mode");
    let (index, _) = occurrences.next()?;
    if occurrences.next().is_some() { None } else { args.get(index + 1).and_then(|value| cli_mode(value)) }
}

pub(crate) fn parse_args(args: &[&str]) -> Result<Request, GateError> {
    if args.len() > 22 || !args.len().is_multiple_of(2) {
        return Err(error(ErrorCode::InvalidArgument));
    }
    let mut flags = BTreeMap::new();
    for [flag, value] in args.as_chunks::<2>().0 {
        if !matches!(
            *flag,
            "--mode"
                | "--tree"
                | "--tree-kind"
                | "--inputs-root"
                | "--approval"
                | "--target"
                | "--expected-revision"
                | "--expected-contract-digest"
                | "--expected-policy-digest"
                | "--expected-sources-digest"
                | "--advertised"
        ) || value.is_empty()
            || value.starts_with("--")
            || value.contains('\0')
            || flags.insert(*flag, *value).is_some()
        {
            return Err(error(ErrorCode::InvalidArgument));
        }
    }
    let mode = flags.get("--mode").and_then(|value| cli_mode(value)).ok_or_else(|| error(ErrorCode::InvalidArgument))?;
    let tree_kind = match flags.get("--tree-kind") {
        None => None,
        Some(&"repository") => Some(TreeKind::Repository),
        Some(&"package") => Some(TreeKind::Package),
        _ => return Err(error(ErrorCode::InvalidArgument)),
    };
    let advertised = match flags.get("--advertised") {
        None => None,
        Some(&"true") => Some(true),
        Some(&"false") => Some(false),
        _ => return Err(error(ErrorCode::InvalidArgument)),
    };
    let text = |name| flags.get(name).map(|value| (*value).to_owned());
    let request = Request {
        mode,
        tree: flags.get("--tree").map(PathBuf::from),
        tree_kind,
        inputs_root: flags.get("--inputs-root").map(PathBuf::from),
        approval_id: text("--approval"),
        target: text("--target"),
        expected_revision: text("--expected-revision"),
        expected_contract_digest: text("--expected-contract-digest"),
        expected_policy_digest: text("--expected-policy-digest"),
        expected_sources_digest: text("--expected-sources-digest"),
        advertised,
    };
    validate_request(&request)?;
    Ok(request)
}

fn validate_request(request: &Request) -> Result<(), GateError> {
    let has_artifact_flags = request.inputs_root.is_some()
        || request.approval_id.is_some()
        || request.expected_contract_digest.is_some()
        || request.expected_policy_digest.is_some()
        || request.expected_sources_digest.is_some();
    let valid = match request.mode {
        Mode::Policy => {
            !has_artifact_flags
                && request.tree.is_none()
                && request.tree_kind.is_none()
                && request.target.is_none()
                && request.expected_revision.is_none()
                && request.advertised.is_none()
        }
        Mode::HostOnly | Mode::Artifact => {
            let common = request.tree.as_ref().is_some_and(|path| !path.as_os_str().is_empty())
                && request.target.as_deref().is_some_and(identifier)
                && request.expected_revision.as_deref().is_some_and(|s| hex(s, 40));
            common
                && if request.mode == Mode::HostOnly {
                    !has_artifact_flags && request.tree_kind == Some(TreeKind::Repository) && request.advertised == Some(false)
                } else {
                    request.tree_kind.is_none()
                        && request.advertised == Some(true)
                        && request.inputs_root.as_ref().is_some_and(|path| !path.as_os_str().is_empty())
                        && request.approval_id.as_deref().is_some_and(identifier)
                        && [
                            request.expected_contract_digest.as_deref(),
                            request.expected_policy_digest.as_deref(),
                            request.expected_sources_digest.as_deref(),
                        ]
                        .iter()
                        .all(|value| value.is_some_and(|s| s.strip_prefix("sha256:").is_some_and(|h| hex(h, 64))))
                }
        }
    };
    if valid { Ok(()) } else { Err(error(ErrorCode::InvalidArgument)) }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ObservedFile {
    pub path: String,
    pub digest: String,
    pub size_bytes: u64,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GateReport {
    pub schema_version: String,
    pub mode: Mode,
    pub status: GateStatus,
    pub target: Option<String>,
    pub tree_kind: Option<TreeKind>,
    pub policy_digest: String,
    pub source_records_digest: String,
    pub inventory_digest: Option<String>,
    pub files_checked: u64,
    pub inputs_checked: u64,
    pub dependencies_checked: u64,
}

/// Serialize only controlled diagnostics; errors never include parser text or absolute input roots.
pub(crate) fn write_result(out: &mut dyn Write, mode: Option<Mode>, result: &Result<GateReport, GateError>) -> Result<(), GateError> {
    match result {
        Ok(report) => serde_json::to_writer(&mut *out, report),
        Err(failure) => {
            #[derive(Serialize)]
            struct Diagnostic<'a> {
                code: &'a ErrorCode,
                subject: Option<&'a str>,
            }
            #[derive(Serialize)]
            #[serde(rename_all = "camelCase")]
            struct Failure<'a> {
                schema_version: &'static str,
                mode: Option<Mode>,
                status: &'static str,
                error: Diagnostic<'a>,
            }
            let subject = failure.subject.as_deref().filter(|s| matches!(path_check(s), Ok(()) | Err("ENTERPRISE_INPUT")));
            serde_json::to_writer(
                &mut *out,
                &Failure { schema_version: "1.0", mode, status: "failed", error: Diagnostic { code: &failure.code, subject } },
            )
        }
    }
    .map_err(|_| error(ErrorCode::IoError))?;
    out.write_all(b"\n").and_then(|()| out.flush()).map_err(|_| error(ErrorCode::IoError))
}

/// Thin stdio adapter: emitting a failure report never changes the failed command to success.
pub(crate) fn run(root: &Path, args: &[&str]) -> Result<(), String> {
    let result = parse_args(args).and_then(|request| evaluate(root, &request));
    write_result(&mut std::io::stdout().lock(), identifiable_mode(args), &result).map_err(|e| e.code.as_str().to_owned())?;
    result.map(|_| ()).map_err(|e| e.code.as_str().to_owned())
}

#[derive(Clone, Copy)]
struct TreeLimits {
    entries: usize,
    file_bytes: u64,
    total_bytes: u64,
}

const TREE_LIMITS: TreeLimits = TreeLimits { entries: MAX_ENTRIES, file_bytes: MAX_FILE_BYTES, total_bytes: MAX_TOTAL_BYTES };

#[derive(PartialEq, Eq)]
enum TreeScope {
    Exact,
    Repository,
}

struct TreeEntry {
    path: String,
    metadata: std::fs::Metadata,
}

fn at(code: ErrorCode, path: &str) -> GateError {
    GateError { code, subject: Some(path.to_owned()) }
}

/// Check every ancestor from the filesystem root down, before resolving/opening below it.
fn directory_root(root: &Path) -> Result<PathBuf, GateError> {
    let absolute = if root.is_absolute() { root.to_owned() } else { std::env::current_dir().map_err(|_| error(ErrorCode::IoError))?.join(root) };
    let checked = directory_ancestors(&absolute, |path| Ok(std::fs::symlink_metadata(path).map_err(|_| error(ErrorCode::IoError))?.is_dir()))?;
    std::fs::canonicalize(checked).map_err(|_| error(ErrorCode::IoError))
}

fn directory_step(
    checked: &mut PathBuf,
    part: &std::ffi::OsStr,
    prefix_only: bool,
    is_directory: &mut impl FnMut(&Path) -> Result<bool, GateError>,
) -> Result<(), GateError> {
    checked.push(part);
    // A Windows prefix alone (e.g. \\?\C:) is not a directory. Check prefix + RootDir next.
    if !prefix_only && !is_directory(checked)? {
        return Err(error(ErrorCode::UnsafePath));
    }
    Ok(())
}

fn directory_ancestors(absolute: &Path, mut is_directory: impl FnMut(&Path) -> Result<bool, GateError>) -> Result<PathBuf, GateError> {
    let mut checked = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => return Err(error(ErrorCode::UnsafePath)),
            Component::CurDir => continue,
            _ => directory_step(&mut checked, component.as_os_str(), matches!(component, Component::Prefix(_)), &mut is_directory)?,
        }
    }
    Ok(checked)
}

fn same_metadata(before: &std::fs::Metadata, after: &std::fs::Metadata) -> Result<bool, GateError> {
    let same = before.file_type() == after.file_type()
        && before.len() == after.len()
        && before.permissions() == after.permissions()
        && before.modified().map_err(|_| error(ErrorCode::IoError))? == after.modified().map_err(|_| error(ErrorCode::IoError))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(same
            && before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec())
    }
    #[cfg(not(unix))]
    Ok(same)
}

fn excluded(path: &str, metadata: &std::fs::Metadata, scope: &TreeScope) -> bool {
    *scope == TreeScope::Repository
        && (path == ".git"
            || (!path.contains('/') && path.starts_with("target"))
            || (metadata.is_dir() && path.rsplit('/').next() == Some("node_modules")))
}

fn runtime_marker(entry: &TreeEntry) -> bool {
    let name = entry.path.rsplit('/').next().unwrap_or_default();
    name.eq_ignore_ascii_case("runtime.json")
        || (entry.metadata.is_file() && (name.eq_ignore_ascii_case("node") || name.eq_ignore_ascii_case("node.exe")))
        || (entry.metadata.is_dir()
            && ((name.eq_ignore_ascii_case("mastra") && entry.path != "packaging/mastra")
                || entry.path.eq_ignore_ascii_case("packages/mastra-runtime/dist")))
}

fn add_directory(root: &Path, relative: &str, pending: &mut BTreeSet<String>, count: &mut usize, limits: TreeLimits) -> Result<(), GateError> {
    for entry in std::fs::read_dir(root.join(relative)).map_err(|_| error(ErrorCode::IoError))? {
        // Bound directories too: empty directories must not defeat the allocation/work cap.
        *count = count.checked_add(1).filter(|n| *n <= limits.entries).ok_or_else(|| error(ErrorCode::LimitExceeded))?;
        let entry = entry.map_err(|_| error(ErrorCode::IoError))?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| error(ErrorCode::UnsafePath))?;
        let length = relative.len().checked_add(name.len()).and_then(|n| n.checked_add(usize::from(!relative.is_empty())));
        if length.is_none_or(|n| n > 1024) {
            return Err(error(ErrorCode::UnsafePath));
        }
        pending.insert(if relative.is_empty() { name.to_owned() } else { format!("{relative}/{name}") });
    }
    Ok(())
}

fn collect_tree(root: &Path, scope: &TreeScope, limits: TreeLimits) -> Result<Vec<TreeEntry>, GateError> {
    let mut pending = BTreeSet::new();
    let mut count = 0;
    add_directory(root, "", &mut pending, &mut count, limits)?;
    let mut folded = BTreeSet::new();
    let mut entries = Vec::new();
    while let Some(path) = pending.pop_first() {
        // Enterprise checks follow ALL path/type checks, and precede ALL content reads.
        match path_check(&path) {
            Ok(()) | Err("ENTERPRISE_INPUT") => {}
            Err(_) => return Err(error(ErrorCode::UnsafePath)),
        }
        if !folded.insert(path.to_ascii_lowercase()) {
            return Err(error(ErrorCode::UnsafePath));
        }
        let metadata = std::fs::symlink_metadata(root.join(&path)).map_err(|_| at(ErrorCode::IoError, &path))?;
        if !metadata.is_file() && !metadata.is_dir() {
            return Err(at(ErrorCode::UnsafePath, &path));
        }
        if excluded(&path, &metadata, scope) {
            continue;
        }
        if metadata.is_dir() {
            add_directory(root, &path, &mut pending, &mut count, limits)?;
        }
        entries.push(TreeEntry { path, metadata });
    }
    for entry in &entries {
        path_check(&entry.path).map_err(parse_error)?;
    }
    if *scope == TreeScope::Repository
        && let Some(entry) = entries.iter().find(|entry| runtime_marker(entry))
    {
        return Err(at(ErrorCode::RuntimePresent, &entry.path));
    }
    let mut total = 0_u64;
    for entry in entries.iter().filter(|entry| entry.metadata.is_file()) {
        if entry.metadata.len() > limits.file_bytes {
            return Err(at(ErrorCode::LimitExceeded, &entry.path));
        }
        total =
            total.checked_add(entry.metadata.len()).filter(|n| *n <= limits.total_bytes).ok_or_else(|| at(ErrorCode::LimitExceeded, &entry.path))?;
    }
    Ok(entries)
}

fn unchanged(root: &Path, entry: &TreeEntry) -> Result<(), GateError> {
    let after = std::fs::symlink_metadata(root.join(&entry.path)).map_err(|_| at(ErrorCode::IoError, &entry.path))?;
    if !after.is_file() && !after.is_dir() {
        return Err(at(ErrorCode::UnsafePath, &entry.path));
    }
    if !same_metadata(&entry.metadata, &after)? {
        return Err(at(ErrorCode::DigestMismatch, &entry.path));
    }
    Ok(())
}

fn inspect_using<R: Read>(
    root: &Path,
    scope: TreeScope,
    limits: TreeLimits,
    mut open: impl FnMut(&Path) -> std::io::Result<R>,
) -> Result<Vec<ObservedFile>, GateError> {
    let root = directory_root(root)?;
    let before = std::fs::symlink_metadata(&root).map_err(|_| error(ErrorCode::IoError))?;
    let entries = collect_tree(&root, &scope, limits)?;
    inspect_collected(&root, &before, &entries, limits, &mut open)
}

fn inspect_collected<R: Read>(
    root: &Path,
    before: &std::fs::Metadata,
    entries: &[TreeEntry],
    limits: TreeLimits,
    open: &mut impl FnMut(&Path) -> std::io::Result<R>,
) -> Result<Vec<ObservedFile>, GateError> {
    // Preserve the original path/type snapshot across approval and semantic validation too.
    for entry in entries {
        unchanged(root, entry)?;
    }
    let mut files = Vec::new();
    let mut total = 0_u64;
    for entry in entries.iter().filter(|entry| entry.metadata.is_file()) {
        files.push(hash_entry(root, entry, limits, &mut total, open)?);
    }
    for entry in entries {
        unchanged(root, entry)?;
    }
    let after = std::fs::symlink_metadata(root).map_err(|_| error(ErrorCode::IoError))?;
    if !same_metadata(before, &after)? {
        return Err(error(ErrorCode::DigestMismatch));
    }
    Ok(files)
}

fn hash_entry<R: Read>(
    root: &Path,
    entry: &TreeEntry,
    limits: TreeLimits,
    total: &mut u64,
    open: &mut impl FnMut(&Path) -> std::io::Result<R>,
) -> Result<ObservedFile, GateError> {
    unchanged(root, entry)?;
    // Shared with tree inspection: fixed buffer, sentinel bound, checked counts and metadata recheck.
    let bound = entry.metadata.len().checked_add(1).ok_or_else(|| at(ErrorCode::LimitExceeded, &entry.path))?;
    let mut reader = open(&root.join(&entry.path)).map_err(|_| at(ErrorCode::IoError, &entry.path))?.take(bound);
    let mut hash = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer).map_err(|_| at(ErrorCode::IoError, &entry.path))?;
        if count == 0 {
            break;
        }
        size = size.checked_add(count as u64).filter(|n| *n <= limits.file_bytes).ok_or_else(|| at(ErrorCode::LimitExceeded, &entry.path))?;
        *total = total.checked_add(count as u64).filter(|n| *n <= limits.total_bytes).ok_or_else(|| at(ErrorCode::LimitExceeded, &entry.path))?;
        hash.update(buffer.get(..count).ok_or_else(|| at(ErrorCode::IoError, &entry.path))?);
    }
    if size != entry.metadata.len() {
        return Err(at(ErrorCode::DigestMismatch, &entry.path));
    }
    unchanged(root, entry)?;
    Ok(ObservedFile { path: entry.path.clone(), digest: format!("sha256:{:x}", hash.finalize()), size_bytes: size })
}

/// Exact file observations, with NO repository/cache exemptions. The caller freezes the stage.
#[cfg(test)]
pub(crate) fn inspect_tree(root: &Path) -> Result<Vec<ObservedFile>, GateError> {
    inspect_using(root, TreeScope::Exact, TREE_LIMITS, |path| std::fs::File::open(path))
}

pub(crate) fn evaluate(root: &Path, request: &Request) -> Result<GateReport, GateError> {
    let root = directory_root(root)?;
    check_repository_policy(&root)?;
    validate_request(request)?;
    if request.mode == Mode::Policy {
        let (_, policy_digest) = read_metadata_digest::<Policy>(&root, POLICY_PATH)?;
        let (_, source_records_digest) = read_metadata_digest::<SourceRecords>(&root, SOURCES_PATH)?;
        return Ok(GateReport {
            schema_version: "1.0".into(),
            mode: Mode::Policy,
            status: GateStatus::PolicyValid,
            target: None,
            tree_kind: None,
            policy_digest,
            source_records_digest,
            inventory_digest: None,
            files_checked: 0,
            inputs_checked: 0,
            dependencies_checked: 0,
        });
    }
    if request.mode == Mode::Artifact {
        return evaluate_artifact(&root, request);
    }
    let tree = request.tree.as_ref().ok_or_else(|| error(ErrorCode::InvalidArgument))?;
    let tree = directory_root(&root.join(tree))?;
    if tree != root {
        return Err(error(ErrorCode::InvalidArgument));
    }
    // Request carries an expected revision, but #17 owns trusted immutable-checkout validation.
    // This result neither attests Git identity nor grants runtime availability/Node coverage.
    let files = inspect_using(&tree, TreeScope::Repository, TREE_LIMITS, |path| std::fs::File::open(path))?;
    let control_digest =
        |path| files.iter().find(|file| file.path == path).map(|file| file.digest.clone()).ok_or_else(|| at(ErrorCode::IoError, path));
    Ok(GateReport {
        schema_version: "1.0".into(),
        mode: Mode::HostOnly,
        status: GateStatus::NotApplicable,
        target: request.target.clone(),
        tree_kind: Some(TreeKind::Repository),
        policy_digest: control_digest(POLICY_PATH)?,
        source_records_digest: control_digest(SOURCES_PATH)?,
        inventory_digest: None,
        files_checked: files.len() as u64,
        inputs_checked: 0,
        dependencies_checked: 0,
    })
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn matches_digest(observed: &str, expected: &str, subject: &str) -> Result<(), GateError> {
    if observed == expected { Ok(()) } else { Err(at(ErrorCode::DigestMismatch, subject)) }
}

fn provenance(condition: bool, subject: &str) -> Result<(), GateError> {
    if condition { Ok(()) } else { Err(at(ErrorCode::ProvenanceIncomplete, subject)) }
}

fn artifact_target(value: &str) -> Result<Target, GateError> {
    match value {
        "aarch64-apple-darwin" => Ok(Target::Aarch64Apple),
        "x86_64-apple-darwin" => Ok(Target::X86Apple),
        "x86_64-pc-windows-msvc" => Ok(Target::X86Windows),
        "x86_64-unknown-linux-gnu" => Ok(Target::X86Linux),
        _ => Err(error(ErrorCode::UnsupportedTarget)),
    }
}

// Trusted root/SHA and digest expectations are supplied by the immutable-checkout CI caller (#17).
// Comparing receipt JSON alone does NOT establish the actual checkout's Git identity or cleanliness.
fn evaluate_artifact(root: &Path, request: &Request) -> Result<GateReport, GateError> {
    if request.tree_kind.is_some()
        || request.advertised != Some(true)
        || !request.approval_id.as_deref().is_some_and(identifier)
        || !request.expected_revision.as_deref().is_some_and(|s| hex(s, 40))
        || [request.expected_contract_digest.as_deref(), request.expected_policy_digest.as_deref(), request.expected_sources_digest.as_deref()]
            .iter()
            .any(|s| !s.is_some_and(|s| s.strip_prefix("sha256:").is_some_and(|h| hex(h, 64))))
    {
        return Err(error(ErrorCode::InvalidArgument));
    }
    let target = artifact_target(request.target.as_deref().ok_or_else(|| error(ErrorCode::InvalidArgument))?)?;
    let tree = directory_root(&root.join(request.tree.as_ref().ok_or_else(|| error(ErrorCode::InvalidArgument))?))?;
    let inputs_root = directory_root(&root.join(request.inputs_root.as_ref().ok_or_else(|| error(ErrorCode::InvalidArgument))?))?;
    // No host-only .git/target/node_modules exclusions. Path/type and enterprise rejection precede code reads.
    let artifact_before = std::fs::symlink_metadata(&tree).map_err(|_| error(ErrorCode::IoError))?;
    let artifact_entries = collect_tree(&tree, &TreeScope::Exact, TREE_LIMITS)?;
    let inputs_before = std::fs::symlink_metadata(&inputs_root).map_err(|_| error(ErrorCode::IoError))?;
    let input_entries = collect_tree(&inputs_root, &TreeScope::Exact, TREE_LIMITS)?;
    let (reviewed_policy, policy_digest) = read_metadata_digest::<Policy>(root, POLICY_PATH)?;
    let (reviewed_sources, sources_digest) = read_metadata_digest::<SourceRecords>(root, SOURCES_PATH)?;
    reviewed_sources.validate_policy(&reviewed_policy)?;
    let (observed_inventory, inventory_digest) = read_metadata_digest::<Inventory>(&tree, "inventory.json")?;
    let (receipt, receipt_digest) = read_metadata_digest::<RuntimeReceipt>(&tree, "runtime.json")?;
    if !reviewed_policy.supported_targets.0.contains(&target) {
        return Err(error(ErrorCode::UnsupportedTarget));
    }
    let approval = reviewed_policy
        .approvals
        .0
        .iter()
        .find(|a| Some(a.id.0.as_str()) == request.approval_id.as_deref())
        .ok_or_else(|| error(ErrorCode::UnapprovedArtifact))?;
    if approval.target != target || approval.inventory_digest.0 != inventory_digest {
        return Err(at(ErrorCode::UnapprovedArtifact, &approval.id.0));
    }
    for (observed, expected, subject) in [
        (policy_digest.as_str(), request.expected_policy_digest.as_deref(), POLICY_PATH),
        (sources_digest.as_str(), request.expected_sources_digest.as_deref(), SOURCES_PATH),
        (receipt.policy_digest.0.as_str(), Some(policy_digest.as_str()), POLICY_PATH),
        (receipt.source_records_digest.0.as_str(), Some(sources_digest.as_str()), SOURCES_PATH),
        (receipt.contract_digest.0.as_str(), request.expected_contract_digest.as_deref(), "runtime.json"),
        (receipt.provenance_inventory.digest.0.as_str(), Some(inventory_digest.as_str()), "inventory.json"),
        (observed_inventory.contract_manifest.digest.0.as_str(), request.expected_contract_digest.as_deref(), "inventory.json"),
    ] {
        matches_digest(observed, expected.ok_or_else(|| error(ErrorCode::InvalidArgument))?, subject)?;
    }
    if observed_inventory.target != target
        || receipt.target_triple != target
        || receipt.runtime_version != observed_inventory.runtime_version
        || receipt.node_version != observed_inventory.node.version
        || receipt.protocol_versions != observed_inventory.protocol_versions
        || Some(receipt.repository_revision.0.as_str()) != request.expected_revision.as_deref()
    {
        return Err(error(ErrorCode::IdentityMismatch));
    }
    if !receipt.artifact_digests.0.iter().map(|f| (&f.path, &f.digest)).eq(observed_inventory.outputs.0.iter().map(|o| (&o.path, &o.digest))) {
        return Err(at(ErrorCode::DigestMismatch, "runtime.json"));
    }

    // All declared rights/identity/registry/graph checks finish BEFORE opening source or output code.
    let dependencies_checked = observed_inventory.validate_artifact_semantics(&reviewed_policy, &reviewed_sources)?;
    let mut repository_files = BTreeMap::new();
    for reference in [&observed_inventory.lockfile, &observed_inventory.contract_manifest] {
        provenance(!control_path(&reference.path.0), &reference.path.0)?;
        add_repository_file(&mut repository_files, &reference.path.0, &reference.digest.0, None)?;
    }
    for input in &observed_inventory.inputs.0 {
        if let Some(id) = input.source_record_id.0.as_ref() {
            let record = reviewed_sources
                .records
                .0
                .binary_search_by(|r| r.id.cmp(id))
                .ok()
                .and_then(|index| reviewed_sources.records.0.get(index))
                .ok_or_else(|| at(ErrorCode::SourceRightsUnverified, &id.0))?;
            let destination = record.destination_path.0.as_ref().ok_or_else(|| at(ErrorCode::SourceRightsUnverified, &id.0))?;
            add_repository_file(&mut repository_files, &destination.0, &input.digest.0, Some(input.size_bytes.0))?;
            for notice in &record.required_notices.0 {
                add_repository_file(&mut repository_files, &notice.path.0, &notice.digest.0, None)?;
            }
        } else if input.origin_kind == OriginKind::Repository {
            add_repository_file(&mut repository_files, &input.original_path.0, &input.digest.0, Some(input.size_bytes.0))?;
        }
    }
    let mut expected_files: BTreeSet<_> = observed_inventory.outputs.0.iter().map(|o| o.path.0.as_str()).collect();
    expected_files.extend(["inventory.json", "runtime.json"]);
    exact_files(&artifact_entries, &expected_files)?;
    exact_files(&input_entries, &observed_inventory.inputs.0.iter().map(|i| i.materialized_path.0.as_str()).collect())?;

    // Streaming inspections reuse Task3's bounds, metadata rechecks and exact (not host-only) scope.
    check_repository_files(root, &repository_files)?;
    let files = inspect_collected(&tree, &artifact_before, &artifact_entries, TREE_LIMITS, &mut |path| std::fs::File::open(path))?;
    let inputs = inspect_collected(&inputs_root, &inputs_before, &input_entries, TREE_LIMITS, &mut |path| std::fs::File::open(path))?;
    let files_by_path: BTreeMap<_, _> = files.iter().map(|f| (f.path.as_str(), f)).collect();
    let inputs_by_path: BTreeMap<_, _> = inputs.iter().map(|f| (f.path.as_str(), f)).collect();
    for (path, digest) in [("inventory.json", inventory_digest.as_str()), ("runtime.json", receipt_digest.as_str())] {
        matches_digest(&files_by_path.get(path).ok_or_else(|| at(ErrorCode::FileSetMismatch, path))?.digest, digest, path)?;
    }
    for output in &observed_inventory.outputs.0 {
        check_observation(files_by_path.get(output.path.0.as_str()).copied(), &output.path.0, &output.digest.0, output.size_bytes.0)?;
    }
    for input in &observed_inventory.inputs.0 {
        check_observation(
            inputs_by_path.get(input.materialized_path.0.as_str()).copied(),
            &input.materialized_path.0,
            &input.digest.0,
            input.size_bytes.0,
        )?;
    }
    observed_inventory.validate_build_evidence(&inputs_root)?;
    Ok(GateReport {
        schema_version: "1.0".into(),
        mode: Mode::Artifact,
        status: GateStatus::Verified,
        target: request.target.clone(),
        tree_kind: Some(TreeKind::Package),
        policy_digest,
        source_records_digest: sources_digest,
        inventory_digest: Some(inventory_digest),
        files_checked: files.len() as u64,
        inputs_checked: inputs.len() as u64,
        dependencies_checked,
    })
}

fn exact_files(entries: &[TreeEntry], expected: &BTreeSet<&str>) -> Result<(), GateError> {
    let observed: BTreeSet<_> = entries.iter().filter(|e| e.metadata.is_file()).map(|e| e.path.as_str()).collect();
    if let Some(path) = observed.symmetric_difference(expected).next() {
        return Err(at(ErrorCode::FileSetMismatch, path));
    }
    Ok(())
}

fn check_observation(file: Option<&ObservedFile>, path: &str, digest: &str, size: u64) -> Result<(), GateError> {
    let file = file.ok_or_else(|| at(ErrorCode::FileSetMismatch, path))?;
    if file.size_bytes != size {
        return Err(at(ErrorCode::DigestMismatch, path));
    }
    matches_digest(&file.digest, digest, path)
}

fn file_entry(root: &Path, relative: &str) -> Result<TreeEntry, GateError> {
    path_check(relative).map_err(parse_error)?;
    let parent = Path::new(relative).parent().ok_or_else(|| error(ErrorCode::UnsafePath))?;
    directory_root(&root.join(parent))?;
    let metadata = std::fs::symlink_metadata(root.join(relative)).map_err(|_| at(ErrorCode::IoError, relative))?;
    if !metadata.is_file() {
        return Err(at(ErrorCode::UnsafePath, relative));
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(at(ErrorCode::LimitExceeded, relative));
    }
    Ok(TreeEntry { path: relative.to_owned(), metadata })
}

fn add_repository_file<'a>(
    files: &mut BTreeMap<&'a str, (&'a str, Option<u64>)>,
    path: &'a str,
    digest: &'a str,
    size: Option<u64>,
) -> Result<(), GateError> {
    if let Some((previous_digest, previous_size)) = files.get(path) {
        matches_digest(digest, previous_digest, path)?;
        if previous_size.is_some() && size.is_some() && *previous_size != size {
            return Err(at(ErrorCode::DigestMismatch, path));
        }
        if size.is_none() {
            return Ok(());
        }
    }
    files.insert(path, (digest, size));
    if files.len() > MAX_ENTRIES {
        return Err(error(ErrorCode::LimitExceeded));
    }
    Ok(())
}

fn check_repository_files(root: &Path, files: &BTreeMap<&str, (&str, Option<u64>)>) -> Result<(), GateError> {
    // Deduplicate retained notices and shared destinations; no per-input rehash amplification.
    // Check ALL repository path/types/budgets before opening any repository source bytes.
    let mut paths: Vec<_> = files.keys().copied().collect();
    // Group casefolded prefixes, including a file reference beside its descendants. The
    // trailing separator prevents unrelated sibling names from splitting that group.
    paths.sort_by_cached_key(|path| format!("{}/", path.to_ascii_lowercase()));
    for (left, right) in paths.iter().zip(paths.iter().skip(1)) {
        for (a, b) in left.split('/').zip(right.split('/')) {
            if !a.eq_ignore_ascii_case(b) {
                break;
            }
            if a != b {
                return Err(at(ErrorCode::UnsafePath, right));
            }
        }
    }
    let mut entries = Vec::new();
    let mut declared = 0_u64;
    for path in files.keys() {
        let entry = file_entry(root, path)?;
        declared = declared.checked_add(entry.metadata.len()).filter(|n| *n <= MAX_TOTAL_BYTES).ok_or_else(|| at(ErrorCode::LimitExceeded, path))?;
        entries.push(entry);
    }
    let mut total = 0_u64;
    for entry in &entries {
        let (digest, size) = files.get(entry.path.as_str()).ok_or_else(|| at(ErrorCode::ProvenanceIncomplete, &entry.path))?;
        let file = hash_entry(root, entry, TREE_LIMITS, &mut total, &mut |path| std::fs::File::open(path))?;
        check_observation(Some(&file), &entry.path, digest, size.unwrap_or(file.size_bytes))?;
    }
    for entry in &entries {
        unchanged(root, entry)?;
    }
    Ok(())
}

fn control_path(path: &str) -> bool {
    [POLICY_PATH, SOURCES_PATH, "runtime.json", "inventory.json"].iter().any(|control| path.eq_ignore_ascii_case(control))
}

fn dependency_reachable<'a>(
    registry: &BTreeMap<&'a str, &'a Dependency>,
    roots: impl IntoIterator<Item = &'a str>,
) -> Result<BTreeSet<&'a str>, GateError> {
    let mut visited = BTreeSet::new();
    let mut pending: BTreeSet<_> = roots.into_iter().collect();
    if pending.len() > MAX_ENTRIES {
        return Err(error(ErrorCode::LimitExceeded));
    }
    while let Some(id) = pending.pop_first() {
        if !visited.insert(id) {
            continue;
        }
        if visited.len() > MAX_ENTRIES {
            return Err(error(ErrorCode::LimitExceeded));
        }
        let dependency = registry.get(id).ok_or_else(|| at(ErrorCode::ProvenanceIncomplete, id))?;
        for child in &dependency.dependency_ids.0 {
            if !visited.contains(child.0.as_str()) {
                pending.insert(child.0.as_str());
                if pending.len() > MAX_ENTRIES {
                    return Err(error(ErrorCode::LimitExceeded));
                }
            }
        }
    }
    Ok(visited)
}

fn dependency_closure(dependencies: &List<Dependency>, roots: &List<Id>) -> Result<u64, GateError> {
    let registry: BTreeMap<_, _> = dependencies.0.iter().map(|d| (d.id.0.as_str(), d)).collect();
    let visited = dependency_reachable(&registry, roots.0.iter().map(|id| id.0.as_str()))?;
    for id in registry.keys() {
        provenance(visited.contains(id), id)?;
    }
    Ok(visited.len() as u64)
}

impl Inventory {
    fn validate_artifact_semantics(&self, policy: &Policy, sources: &SourceRecords) -> Result<u64, GateError> {
        // Current-build identity is outside the approval-hashed Inventory and precedes rights checks.
        // A migrated repository input is deliberately NOT included in this required-null rule.
        for input in &self.inputs.0 {
            if input.source_record_id.0.is_none()
                && matches!(input.origin_kind, OriginKind::Repository | OriginKind::BuildEvidence)
                && input.source_revision.0.is_some()
            {
                return Err(at(ErrorCode::IdentityMismatch, &input.id.0));
            }
        }
        self.validate_sources(policy, sources)?;
        let dependencies: BTreeSet<_> = self.dependencies.0.iter().map(|d| d.id.0.as_str()).collect();
        let tools: BTreeSet<_> = self.build_tools.0.iter().map(|d| d.id.0.as_str()).collect();
        let records: BTreeMap<_, _> = sources.records.0.iter().map(|r| (r.id.0.as_str(), r)).collect();
        let inputs: BTreeMap<_, _> = self.inputs.0.iter().map(|i| (i.id.0.as_str(), i)).collect();
        let outputs: BTreeMap<_, _> = self.outputs.0.iter().map(|o| (o.path.0.as_str(), o)).collect();
        let licenses: BTreeMap<_, _> = self.licenses.0.iter().map(|l| (l.id.0.as_str(), l)).collect();
        let mut used_licenses = BTreeSet::new();
        let mut used_dependencies = BTreeSet::new();
        let mut required_dependencies = BTreeSet::new();
        let mut used_tools = BTreeSet::new();
        for input in &self.inputs.0 {
            let id = input.id.0.as_str();
            provenance(!control_path(&input.original_path.0) && !control_path(&input.materialized_path.0), id)?;
            if let Some(record_id) = input.source_record_id.0.as_ref() {
                let record = records.get(record_id.0.as_str()).ok_or_else(|| at(ErrorCode::SourceRightsUnverified, &record_id.0))?;
                if input.origin_id != *record_id
                    || input.original_path != record.source_path
                    || record.destination_digest.0.as_ref() != Some(&input.digest)
                    || !matches!(input.origin_kind, OriginKind::Repository | OriginKind::Dependency | OriginKind::Tool)
                    || record.destination_path.0.as_ref().is_some_and(|p| control_path(&p.0))
                {
                    return Err(at(ErrorCode::SourceRightsUnverified, &record_id.0));
                }
                required_dependencies.extend(record.dependency_ids.0.iter().map(|id| id.0.as_str()));
            } else {
                match input.origin_kind {
                    OriginKind::Repository | OriginKind::BuildEvidence => {
                        provenance(input.origin_id == input.id, id)?;
                        if input.origin_kind == OriginKind::BuildEvidence {
                            provenance(input.original_path == input.materialized_path, id)?;
                        }
                    }
                    // External revision pins remain intact in the exact reviewed inventory. Null is
                    // valid for release archives, NEVER a waiver of version/archive/integrity checks.
                    OriginKind::Dependency => {
                        provenance(dependencies.contains(input.origin_id.0.as_str()), id)?;
                        used_dependencies.insert(input.origin_id.0.as_str());
                    }
                    OriginKind::Tool => {
                        provenance(input.origin_id.0 == "package-manager" || tools.contains(input.origin_id.0.as_str()), id)?;
                        used_tools.insert(input.origin_id.0.as_str());
                    }
                    OriginKind::Node => provenance(input.origin_id.0 == "node", id)?,
                }
            }
            for license in &input.license_ids.0 {
                provenance(licenses.contains_key(license.0.as_str()), id)?;
                if matches!(input.origin_kind, OriginKind::Tool | OriginKind::BuildEvidence) {
                    used_licenses.insert(license.0.as_str());
                }
            }
        }
        let mut license_files = BTreeSet::new();
        let mut notice_files = BTreeSet::new();
        for license in &self.licenses.0 {
            for (references, role, referenced) in
                [(&license.license_files, OutputRole::License, &mut license_files), (&license.notice_files, OutputRole::Notice, &mut notice_files)]
            {
                for reference in &references.0 {
                    let output = outputs.get(reference.path.0.as_str()).ok_or_else(|| at(ErrorCode::ProvenanceIncomplete, &reference.path.0))?;
                    provenance(
                        output.role == role && output.digest == reference.digest && output.license_ids.0.contains(&license.id),
                        &reference.path.0,
                    )?;
                    referenced.insert(reference.path.0.as_str());
                }
            }
        }
        for output in &self.outputs.0 {
            let path = output.path.0.as_str();
            provenance(!control_path(path), path)?;
            for license in &output.license_ids.0 {
                provenance(licenses.contains_key(license.0.as_str()), path)?;
                if !matches!(output.role, OutputRole::License | OutputRole::Notice) {
                    used_licenses.insert(license.0.as_str());
                }
            }
            if output.role == OutputRole::License {
                provenance(license_files.contains(path), path)?;
            }
            if output.role == OutputRole::Notice {
                provenance(notice_files.contains(path), path)?;
            }
        }
        for id in licenses.keys() {
            provenance(used_licenses.contains(id), id)?;
        }
        let node =
            outputs.get(self.node.executable_path.0.as_str()).ok_or_else(|| at(ErrorCode::ProvenanceIncomplete, &self.node.executable_path.0))?;
        provenance(
            node.role == OutputRole::Node && self.outputs.0.iter().filter(|o| o.role == OutputRole::Node).count() == 1,
            &self.node.executable_path.0,
        )?;
        // Every used source record needs actual dependency owners, even for copied
        // outputs. Record metadata is a requirement, never materialized input bytes.
        let dependency_registry: BTreeMap<_, _> = self.dependencies.0.iter().map(|d| (d.id.0.as_str(), d)).collect();
        for id in dependency_reachable(&dependency_registry, required_dependencies)? {
            provenance(used_dependencies.contains(id), id)?;
        }
        for id in &dependencies {
            provenance(used_dependencies.contains(id), id)?;
        }
        for id in &tools {
            provenance(used_tools.contains(id), id)?;
        }
        let dependencies_checked = dependency_closure(&self.dependencies, &self.root_dependency_ids)?;
        dependency_closure(&self.build_tools, &self.root_tool_ids)?;
        self.validate_derivation_closure(&inputs, &outputs, &records)?;
        Ok(dependencies_checked)
    }

    // Metadata/rights/graph validation precedes payload hashing. Evidence is interpreted
    // separately, only after every materialized input has matched its approved byte digest.
    fn validate_derivation_closure(
        &self,
        inputs: &BTreeMap<&str, &BuildInput>,
        outputs: &BTreeMap<&str, &Output>,
        records: &BTreeMap<&str, &SourceRecord>,
    ) -> Result<(), GateError> {
        provenance(self.derivations.0.len() == outputs.len(), "inventory.json")?;
        let dependencies: BTreeMap<_, _> = self.dependencies.0.iter().map(|d| (d.id.0.as_str(), d)).collect();
        let tools: BTreeMap<_, _> = self.build_tools.0.iter().map(|d| (d.id.0.as_str(), d)).collect();
        let mut used = BTreeSet::new();
        for derivation in &self.derivations.0 {
            let path = derivation.output_path.0.as_str();
            let output = outputs.get(path).ok_or_else(|| at(ErrorCode::ProvenanceIncomplete, path))?;
            for id in &derivation.input_ids.0 {
                let input = inputs.get(id.0.as_str()).ok_or_else(|| at(ErrorCode::ProvenanceIncomplete, &id.0))?;
                provenance(!matches!(input.origin_kind, OriginKind::Tool | OriginKind::BuildEvidence), &id.0)?;
                provenance(input.license_ids.0.iter().all(|id| output.license_ids.0.contains(id)), path)?;
                used.insert(id.0.as_str());
            }
            for id in &derivation.tool_input_ids.0 {
                let input = inputs.get(id.0.as_str()).ok_or_else(|| at(ErrorCode::ProvenanceIncomplete, &id.0))?;
                provenance(input.origin_kind == OriginKind::Tool, &id.0)?;
                used.insert(id.0.as_str());
            }
            if derivation.tool_input_ids.0.is_empty() {
                provenance(derivation.input_ids.0.len() == 1 && derivation.evidence_input_id.0.is_none(), path)?;
                let input = derivation
                    .input_ids
                    .0
                    .first()
                    .and_then(|id| inputs.get(id.0.as_str()))
                    .ok_or_else(|| at(ErrorCode::ProvenanceIncomplete, path))?;
                provenance(input.digest == output.digest && input.size_bytes == output.size_bytes && input.license_ids == output.license_ids, path)?;
                if output.role == OutputRole::Node {
                    provenance(input.origin_kind == OriginKind::Node, path)?;
                }
            } else {
                provenance(output.role != OutputRole::Node, path)?;
                let id = derivation.evidence_input_id.0.as_ref().ok_or_else(|| at(ErrorCode::ProvenanceIncomplete, path))?;
                let input = inputs.get(id.0.as_str()).ok_or_else(|| at(ErrorCode::ProvenanceIncomplete, &id.0))?;
                provenance(input.origin_kind == OriginKind::BuildEvidence, &id.0)?;
                used.insert(id.0.as_str());
                // A globally used transitive input can still be absent from THIS bundle.
                // Source-record dependency declarations seed requirements, not materialized bytes.
                let mut data_owners = BTreeSet::new();
                let mut data_roots = BTreeSet::new();
                let mut tool_owners = BTreeSet::new();
                for id in derivation.input_ids.0.iter().chain(&derivation.tool_input_ids.0) {
                    let input = inputs.get(id.0.as_str()).ok_or_else(|| at(ErrorCode::ProvenanceIncomplete, &id.0))?;
                    if let Some(record_id) = input.source_record_id.0.as_ref() {
                        let record = records.get(record_id.0.as_str()).ok_or_else(|| at(ErrorCode::SourceRightsUnverified, &record_id.0))?;
                        data_roots.extend(record.dependency_ids.0.iter().map(|id| id.0.as_str()));
                    } else if input.origin_kind == OriginKind::Dependency {
                        data_owners.insert(input.origin_id.0.as_str());
                        data_roots.insert(input.origin_id.0.as_str());
                    } else if input.origin_kind == OriginKind::Tool && input.origin_id.0 != "package-manager" {
                        tool_owners.insert(input.origin_id.0.as_str());
                    }
                }
                provenance(dependency_reachable(&dependencies, data_roots)?.is_subset(&data_owners), path)?;
                provenance(dependency_reachable(&tools, tool_owners.iter().copied())? == tool_owners, path)?;
            }
        }
        for id in inputs.keys() {
            provenance(used.contains(id), id)?;
        }
        Ok(())
    }

    fn validate_build_evidence(&self, inputs_root: &Path) -> Result<(), GateError> {
        let inputs: BTreeMap<_, _> = self.inputs.0.iter().map(|i| (&i.id, i)).collect();
        for derivation in &self.derivations.0 {
            let Some(id) = derivation.evidence_input_id.0.as_ref() else { continue };
            let input = inputs.get(id).ok_or_else(|| at(ErrorCode::ProvenanceIncomplete, &id.0))?;
            let path = input.materialized_path.0.as_str();
            let (evidence, digest) = read_metadata_digest::<BuildEvidence>(inputs_root, path)?;
            matches_digest(&digest, &input.digest.0, path)?;
            provenance(
                evidence.output_path == derivation.output_path
                    && evidence.input_ids == derivation.input_ids
                    && evidence.tool_input_ids == derivation.tool_input_ids,
                path,
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    pub(crate) fn default_tree_limits() -> (usize, u64, u64) {
        (TREE_LIMITS.entries, TREE_LIMITS.file_bytes, TREE_LIMITS.total_bytes)
    }

    pub(crate) fn directory_step_using(
        checked: &mut PathBuf,
        part: &std::ffi::OsStr,
        prefix_only: bool,
        mut is_directory: impl FnMut(&Path) -> Result<bool, GateError>,
    ) -> Result<(), GateError> {
        directory_step(checked, part, prefix_only, &mut is_directory)
    }

    pub(crate) fn directory_ancestors_using(root: &Path, is_directory: impl FnMut(&Path) -> Result<bool, GateError>) -> Result<PathBuf, GateError> {
        directory_ancestors(root, is_directory)
    }

    // The real collector/hasher with smaller budgets and an injectable file reader, not a fake gate.
    pub(crate) fn inspect_tree_using<R: Read>(
        root: &Path,
        entries: usize,
        file_bytes: u64,
        total_bytes: u64,
        open: impl FnMut(&Path) -> std::io::Result<R>,
    ) -> Result<Vec<ObservedFile>, GateError> {
        inspect_using(root, TreeScope::Exact, TreeLimits { entries, file_bytes, total_bytes }, open)
    }
}

macro_rules! scalar {
    ($name:ident, $check:expr) => {
        #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
        struct $name(String);
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                ($check)(&value).map_err(D::Error::custom)?;
                Ok(Self(value))
            }
        }
    };
}

fn require(condition: bool) -> Result<(), &'static str> {
    if condition { Ok(()) } else { Err("INVALID_METADATA") }
}

fn hex(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn numeric(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) && (value.len() == 1 || !value.starts_with('0'))
}

fn version(value: &str) -> bool {
    let (core, prerelease) = value.split_once('-').map_or((value, None), |(c, p)| (c, Some(p)));
    core.split('.').count() == 3
        && core.split('.').all(numeric)
        && prerelease.is_none_or(|p| {
            p.split('.').all(|part| {
                !part.is_empty()
                    && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    && (!part.bytes().all(|b| b.is_ascii_digit()) || numeric(part))
            })
        })
}

fn path_check(value: &str) -> Result<(), &'static str> {
    if value.is_empty() || value.len() > 1024 || !value.is_ascii() || value.bytes().any(|b| b.is_ascii_control() || b"\\:<>\"|?*".contains(&b)) {
        return Err("UNSAFE_PATH");
    }
    for component in value.split('/') {
        let stem = component.split('.').next().unwrap_or_default().to_ascii_uppercase();
        let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ["COM", "LPT"]
                .iter()
                .any(|prefix| stem.strip_prefix(prefix).is_some_and(|n| n.len() == 1 && matches!(n.as_bytes().first(), Some(b'1'..=b'9'))));
        if component.is_empty() || matches!(component, "." | "..") || component.ends_with(['.', ' ']) || device {
            return Err("UNSAFE_PATH");
        }
    }
    if value.split('/').any(|component| component.eq_ignore_ascii_case("ee")) {
        return Err("ENTERPRISE_INPUT");
    }
    Ok(())
}

scalar!(SchemaVersion, |s: &str| require(s == "1.0"));
scalar!(Digest, |s: &str| require(s.strip_prefix("sha256:").is_some_and(|h| hex(h, 64))));
scalar!(GitRevision, |s: &str| require(hex(s, 40)));
fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes().next().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}
scalar!(Id, |s: &str| require(identifier(s)));
scalar!(Version, |s: &str| require(version(s)));
scalar!(RelPath, path_check);
scalar!(Text, |s: &str| require(!s.trim().is_empty()));
scalar!(HttpsUrl, |s: &str| require(s.strip_prefix("https://").is_some_and(|rest| {
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !authority.is_empty() && !authority.contains('@') && !s.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\\')
})));

/// No default: nullable fields additionally use `required_nullable`, so absence is an error.
#[derive(Debug, PartialEq, Eq)]
#[repr(transparent)]
struct RequiredNullable<T>(Option<T>);

fn required_nullable<'de, T: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> Result<RequiredNullable<T>, D::Error> {
    Option::<T>::deserialize(d).map(RequiredNullable)
}

#[derive(Debug, PartialEq, Eq)]
struct List<T>(Vec<T>);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for List<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Bounded<T>(std::marker::PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for Bounded<T> {
            type Value = List<T>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("bounded array")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element()? {
                    if values.len() == MAX_ENTRIES {
                        return Err(A::Error::custom("LIMIT_EXCEEDED"));
                    }
                    values.push(value);
                }
                Ok(List(values))
            }
        }
        deserializer.deserialize_seq(Bounded(std::marker::PhantomData))
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SizeBytes(u64);
impl<'de> Deserialize<'de> for SizeBytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let size = u64::deserialize(deserializer)?;
        if size > MAX_FILE_BYTES {
            return Err(D::Error::custom("LIMIT_EXCEEDED"));
        }
        Ok(Self(size))
    }
}

macro_rules! model {
    ($visibility:vis $name:ident { $($(#[$attr:meta])* $field:ident: $ty:ty),* $(,)? }) => {
        #[derive(Debug, PartialEq, Eq, Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        $visibility struct $name { $($(#[$attr])* $field: $ty),* }
    };
}

macro_rules! choices {
    ($name:ident { $($variant:ident => $wire:literal),* $(,)? }) => {
        #[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
        enum $name { $(#[serde(rename = $wire)] $variant),* }
    };
}

choices!(Target { Aarch64Apple => "aarch64-apple-darwin", X86Apple => "x86_64-apple-darwin", X86Windows => "x86_64-pc-windows-msvc", X86Linux => "x86_64-unknown-linux-gnu" });
choices!(Role { AgentLoop => "agent-loop", ModelRuntime => "model-runtime", Orchestration => "orchestration" });
choices!(ExcludedRole { Account => "account", Database => "database", ScriptExecution => "script-execution", ServerBootstrap => "server-bootstrap" });
choices!(ReviewStatus { Unverified => "unverified", Verified => "verified" });
choices!(PackageManagerName { Npm => "npm", Pnpm => "pnpm", Yarn => "yarn" });
choices!(OriginKind { Repository => "repository", Dependency => "dependency", Node => "node", Tool => "tool", BuildEvidence => "build-evidence" });
choices!(OutputRole { Code => "code", Node => "node", Data => "data", License => "license", Notice => "notice" });

model!(pub(crate) Policy {
    schema_version: SchemaVersion, policy_id: Id, source_records_path: RelPath,
    supported_targets: List<Target>, allowed_license_expressions: List<Text>,
    forbidden_path_components: List<Text>, approvals: List<Approval>,
});
model!(Approval { id: Id, target: Target, inventory_digest: Digest });
model!(pub(crate) SourceRecords { schema_version: SchemaVersion, authorizations: List<Authorization>, records: List<SourceRecord> });
model!(Authorization {
    id: Id, actor: Text, declaration: Text, proposition: Text, source_repository: HttpsUrl, source_revision: GitRevision,
    eligible_roles: List<Role>, excluded_roles: List<ExcludedRole>, license_grant: Text, evidence: Text,
});
model!(SourceRecord {
    id: Id, authorization_id: Id, role: Role, source_repository: HttpsUrl, source_revision: GitRevision, source_path: RelPath,
    #[serde(deserialize_with = "required_nullable")] source_digest: RequiredNullable<Digest>,
    #[serde(deserialize_with = "required_nullable")] destination_path: RequiredNullable<RelPath>,
    #[serde(deserialize_with = "required_nullable")] destination_digest: RequiredNullable<Digest>,
    #[serde(deserialize_with = "required_nullable")] adaptation: RequiredNullable<Text>,
    #[serde(deserialize_with = "required_nullable")] rights_holder: RequiredNullable<Text>,
    #[serde(deserialize_with = "required_nullable")] license_expression: RequiredNullable<Text>,
    required_notices: List<FileRef>, dependency_ids: List<Id>, third_party_review: Review, verification: Review,
});
model!(Review { status: ReviewStatus, evidence: List<Text> });
model!(FileRef { path: RelPath, digest: Digest });
model!(pub(crate) Inventory {
    schema_version: SchemaVersion, artifact_id: Id, target: Target, runtime_version: Version, protocol_versions: List<SchemaVersion>,
    node: Node, package_manager: PackageManager, lockfile: FileRef, contract_manifest: FileRef,
    root_dependency_ids: List<Id>, dependencies: List<Dependency>, root_tool_ids: List<Id>, build_tools: List<Dependency>,
    licenses: List<LicenseEvidence>, inputs: List<BuildInput>, outputs: List<Output>, derivations: List<Derivation>,
});
model!(Node { version: Version, source_url: HttpsUrl, archive_digest: Digest, executable_path: RelPath });
model!(PackageManager { name: PackageManagerName, version: Version, source_url: HttpsUrl, archive_digest: Digest });
model!(Dependency { id: Id, name: Text, version: Version, source_url: HttpsUrl, archive_digest: Digest, lock_integrity: Text, dependency_ids: List<Id> });
model!(LicenseEvidence { id: Id, expression: Text, rights_holder: Text, license_files: List<FileRef>, notice_files: List<FileRef> });
model!(BuildInput {
    id: Id, materialized_path: RelPath, origin_kind: OriginKind, origin_id: Id, original_path: RelPath,
    #[serde(deserialize_with = "required_nullable")] source_revision: RequiredNullable<GitRevision>,
    digest: Digest, size_bytes: SizeBytes, license_ids: List<Id>,
    #[serde(deserialize_with = "required_nullable")] source_record_id: RequiredNullable<Id>,
});
model!(Output { path: RelPath, digest: Digest, size_bytes: SizeBytes, role: OutputRole, license_ids: List<Id> });
model!(Derivation {
    output_path: RelPath, input_ids: List<Id>, tool_input_ids: List<Id>,
    #[serde(deserialize_with = "required_nullable")] evidence_input_id: RequiredNullable<Id>,
});
model!(BuildEvidence { schema_version: SchemaVersion, output_path: RelPath, input_ids: List<Id>, tool_input_ids: List<Id> });
model!(pub(crate) RuntimeReceipt {
    schema_version: SchemaVersion, runtime_version: Version, protocol_versions: List<SchemaVersion>, target_triple: Target,
    node_version: Version, repository_revision: GitRevision, contract_digest: Digest, policy_digest: Digest,
    source_records_digest: Digest, provenance_inventory: FileRef, artifact_digests: List<FileRef>,
});

fn valid(condition: bool) -> Result<(), GateError> {
    if condition { Ok(()) } else { Err(error(ErrorCode::InvalidMetadata)) }
}

fn sorted<T: Ord>(values: &List<T>) -> Result<(), GateError> {
    valid(values.0.windows(2).all(|pair| matches!(pair, [a, b] if a < b)))
}

fn keys<'a>(values: impl IntoIterator<Item = &'a Id>) -> Result<(), GateError> {
    let mut previous = None;
    for id in values {
        valid(previous.is_none_or(|p| p < id))?;
        previous = Some(id);
    }
    Ok(())
}

fn paths<'a>(values: impl IntoIterator<Item = &'a RelPath>) -> Result<(), GateError> {
    let mut previous = None;
    let mut folded = BTreeSet::new();
    for path in values {
        valid(previous.is_none_or(|p| p < path) && folded.insert(path.0.to_ascii_lowercase()))?;
        previous = Some(path);
    }
    Ok(())
}

fn unique_paths<'a>(values: impl IntoIterator<Item = &'a RelPath>) -> Result<(), GateError> {
    let mut folded = BTreeSet::new();
    for path in values {
        valid(folded.insert(path.0.to_ascii_lowercase()))?;
    }
    Ok(())
}

pub(crate) trait Metadata: DeserializeOwned {
    fn validate(&self) -> Result<(), GateError>;
}

impl Metadata for Policy {
    fn validate(&self) -> Result<(), GateError> {
        valid(self.source_records_path.0 == SOURCES_PATH)?;
        valid(self.forbidden_path_components.0.iter().map(|v| v.0.as_str()).eq(["ee"]))?;
        sorted(&self.supported_targets)?;
        sorted(&self.allowed_license_expressions)?;
        valid(!self.supported_targets.0.is_empty() && !self.allowed_license_expressions.0.is_empty())?;
        keys(self.approvals.0.iter().map(|a| &a.id))?;
        for approval in &self.approvals.0 {
            if !self.supported_targets.0.contains(&approval.target) {
                return Err(GateError { code: ErrorCode::PolicyMismatch, subject: Some(approval.id.0.clone()) });
            }
        }
        Ok(())
    }
}

impl Metadata for SourceRecords {
    fn validate(&self) -> Result<(), GateError> {
        keys(self.authorizations.0.iter().map(|a| &a.id))?;
        keys(self.records.0.iter().map(|r| &r.id))?;
        unique_paths(self.records.0.iter().map(|r| &r.source_path))?;
        unique_paths(self.records.0.iter().filter_map(|r| r.destination_path.0.as_ref()))?;
        for authorization in &self.authorizations.0 {
            sorted(&authorization.eligible_roles)?;
            sorted(&authorization.excluded_roles)?;
            valid(authorization.license_grant.0 == "MIT OR Apache-2.0")?;
            authorization.validate_scope()?;
        }
        for record in &self.records.0 {
            paths(record.required_notices.0.iter().map(|n| &n.path))?;
            sorted(&record.dependency_ids)?;
            sorted(&record.third_party_review.evidence)?;
            sorted(&record.verification.evidence)?;
            let authorization = self
                .authorizations
                .0
                .binary_search_by(|a| a.id.cmp(&record.authorization_id))
                .ok()
                .and_then(|index| self.authorizations.0.get(index));
            let in_scope = authorization.is_some_and(|a| {
                a.source_repository == record.source_repository
                    && a.source_revision == record.source_revision
                    && a.eligible_roles.0.contains(&record.role)
            });
            let audited = record.third_party_review.status != ReviewStatus::Verified || !record.third_party_review.evidence.0.is_empty();
            let verified = record.verification.status != ReviewStatus::Verified
                || (record.source_digest.0.is_some()
                    && record.destination_path.0.is_some()
                    && record.destination_digest.0.is_some()
                    && record.adaptation.0.is_some()
                    && record.rights_holder.0.as_ref().is_some_and(|h| h.0 == "intgral-ai")
                    && record.license_expression.0.is_some()
                    && record.third_party_review.status == ReviewStatus::Verified
                    && !record.verification.evidence.0.is_empty());
            if !in_scope || !audited || !verified {
                return Err(GateError { code: ErrorCode::SourceRightsUnverified, subject: Some(record.id.0.clone()) });
            }
        }
        Ok(())
    }
}

impl Authorization {
    fn validate_scope(&self) -> Result<(), GateError> {
        let roles = [Role::AgentLoop, Role::ModelRuntime, Role::Orchestration];
        let excluded = [ExcludedRole::Account, ExcludedRole::Database, ExcludedRole::ScriptExecution, ExcludedRole::ServerBootstrap];
        // The real relayed declaration is fixed, not an editable per-file attestation.
        if self.id.0 == OWNER_GRANT_ID || self.source_repository.0 == SOURCE_REPOSITORY {
            if self.id.0 != OWNER_GRANT_ID
                || self.actor.0 != "Product owner representing intgral-ai"
                || self.declaration.0 != "有权代表并授权"
                || self.proposition.0 != OWNER_PROPOSITION
                || self.source_repository.0 != SOURCE_REPOSITORY
                || self.source_revision.0 != SOURCE_REVISION
                || self.eligible_roles.0 != roles
                || self.excluded_roles.0 != excluded
                || self.evidence.0 != OWNER_EVIDENCE
            {
                return Err(GateError { code: ErrorCode::PolicyMismatch, subject: Some(self.id.0.clone()) });
            }
        } else if self.eligible_roles.0.is_empty() || self.excluded_roles.0 != excluded {
            return Err(GateError { code: ErrorCode::SourceRightsUnverified, subject: Some(self.id.0.clone()) });
        }
        Ok(())
    }
}

impl SourceRecords {
    fn validate_policy(&self, policy: &Policy) -> Result<(), GateError> {
        for record in &self.records.0 {
            if record.license_expression.0.as_ref().is_some_and(|expression| policy.allowed_license_expressions.0.binary_search(expression).is_err())
            {
                return Err(GateError { code: ErrorCode::SourceRightsUnverified, subject: Some(record.id.0.clone()) });
            }
        }
        Ok(())
    }
}

impl Metadata for Inventory {
    fn validate(&self) -> Result<(), GateError> {
        valid(self.protocol_versions.0.len() == 1)?;
        sorted(&self.root_dependency_ids)?;
        sorted(&self.root_tool_ids)?;
        keys(self.dependencies.0.iter().map(|d| &d.id))?;
        keys(self.build_tools.0.iter().map(|d| &d.id))?;
        keys(self.licenses.0.iter().map(|l| &l.id))?;
        keys(self.inputs.0.iter().map(|i| &i.id))?;
        let owners: Vec<_> = self
            .dependencies
            .0
            .iter()
            .chain(&self.build_tools.0)
            .map(|d| &d.id)
            .chain(self.licenses.0.iter().map(|l| &l.id))
            .chain(self.inputs.0.iter().map(|i| &i.id))
            .collect();
        valid(owners.iter().copied().collect::<BTreeSet<_>>().len() == owners.len())?;
        for dependency in self.dependencies.0.iter().chain(&self.build_tools.0) {
            sorted(&dependency.dependency_ids)?;
        }
        for license in &self.licenses.0 {
            valid(!license.license_files.0.is_empty())?;
            paths(license.license_files.0.iter().map(|f| &f.path))?;
            paths(license.notice_files.0.iter().map(|f| &f.path))?;
        }
        unique_paths(self.inputs.0.iter().map(|i| &i.materialized_path))?;
        paths(self.outputs.0.iter().map(|o| &o.path))?;
        paths(self.derivations.0.iter().map(|d| &d.output_path))?;
        for input in &self.inputs.0 {
            sorted(&input.license_ids)?;
            valid(!input.license_ids.0.is_empty())?;
        }
        for output in &self.outputs.0 {
            sorted(&output.license_ids)?;
            valid(!output.license_ids.0.is_empty())?;
        }
        for derivation in &self.derivations.0 {
            sorted(&derivation.input_ids)?;
            sorted(&derivation.tool_input_ids)?;
            valid(!derivation.input_ids.0.is_empty())?;
        }
        let mut total = 0_u64;
        for size in self.inputs.0.iter().map(|i| i.size_bytes.0).chain(self.outputs.0.iter().map(|o| o.size_bytes.0)) {
            total = total.checked_add(size).filter(|sum| *sum <= MAX_TOTAL_BYTES).ok_or_else(|| error(ErrorCode::LimitExceeded))?;
        }
        Ok(())
    }
}

impl Inventory {
    /// Exact reviewed vocabulary only, not rights/provenance approval; run before payload reads.
    pub(crate) fn validate_licenses(&self, policy: &Policy) -> Result<(), GateError> {
        for license in &self.licenses.0 {
            if !policy.allowed_license_expressions.0.contains(&license.expression) {
                return Err(GateError { code: ErrorCode::SourceRightsUnverified, subject: Some(license.id.0.clone()) });
            }
        }
        Ok(())
    }

    /// Static used-source admission only; artifact byte/closure checks are still required.
    pub(crate) fn validate_sources(&self, policy: &Policy, sources: &SourceRecords) -> Result<(), GateError> {
        sources.validate_policy(policy)?;
        self.validate_licenses(policy)?;
        for input in &self.inputs.0 {
            let Some(id) = input.source_record_id.0.as_ref() else { continue };
            let unverified = || GateError { code: ErrorCode::SourceRightsUnverified, subject: Some(id.0.clone()) };
            let record =
                sources.records.0.binary_search_by(|r| r.id.cmp(id)).ok().and_then(|index| sources.records.0.get(index)).ok_or_else(unverified)?;
            if record.verification.status != ReviewStatus::Verified || input.source_revision.0.as_ref() != Some(&record.source_revision) {
                return Err(unverified());
            }
            let dependencies_accounted = record.dependency_ids.0.iter().all(|id| self.dependencies.0.binary_search_by(|d| d.id.cmp(id)).is_ok());
            let license_accounted = input.license_ids.0.iter().any(|id| {
                self.licenses.0.binary_search_by(|l| l.id.cmp(id)).ok().and_then(|index| self.licenses.0.get(index)).is_some_and(|license| {
                    record.license_expression.0.as_ref() == Some(&license.expression)
                        && record.rights_holder.0.as_ref() == Some(&license.rights_holder)
                        && record.required_notices.0.iter().all(|notice| {
                            license
                                .notice_files
                                .0
                                .binary_search_by(|file| file.path.cmp(&notice.path))
                                .ok()
                                .and_then(|index| license.notice_files.0.get(index))
                                == Some(notice)
                        })
                })
            });
            if !dependencies_accounted || !license_accounted {
                return Err(unverified());
            }
        }
        Ok(())
    }
}

impl Metadata for RuntimeReceipt {
    fn validate(&self) -> Result<(), GateError> {
        valid(self.protocol_versions.0.len() == 1 && self.provenance_inventory.path.0 == "inventory.json")?;
        paths(self.artifact_digests.0.iter().map(|f| &f.path))
    }
}

impl Metadata for BuildEvidence {
    fn validate(&self) -> Result<(), GateError> {
        sorted(&self.input_ids)?;
        sorted(&self.tool_input_ids)
    }
}

fn parse_error(message: &str) -> GateError {
    // Inspect only our fixed error tags, never return serde snippets or offending input.
    error(if message.starts_with("LIMIT_EXCEEDED") {
        ErrorCode::LimitExceeded
    } else if message.starts_with("ENTERPRISE_INPUT") {
        ErrorCode::EnterpriseInput
    } else if message.starts_with("UNSAFE_PATH") {
        ErrorCode::UnsafePath
    } else {
        ErrorCode::InvalidMetadata
    })
}

pub(crate) fn parse_metadata<T: Metadata>(bytes: &[u8]) -> Result<T, GateError> {
    if bytes.len() as u64 > MAX_JSON_BYTES {
        return Err(error(ErrorCode::LimitExceeded));
    }
    let value: T = serde_json::from_slice(bytes).map_err(|e| parse_error(&e.to_string()))?;
    value.validate()?;
    Ok(value)
}

pub(crate) fn read_metadata<T: Metadata>(root: &Path, relative: &str) -> Result<T, GateError> {
    read_metadata_digest(root, relative).map(|(value, _)| value)
}

fn read_metadata_digest<T: Metadata>(root: &Path, relative: &str) -> Result<(T, String), GateError> {
    path_check(relative).map_err(parse_error)?;
    let result = (|| {
        let entry = file_entry(root, relative)?;
        if entry.metadata.len() > MAX_JSON_BYTES {
            return Err(error(ErrorCode::LimitExceeded));
        }
        let file = std::fs::File::open(root.join(relative)).map_err(|_| error(ErrorCode::IoError))?;
        let mut bytes = Vec::new();
        file.take(MAX_JSON_BYTES + 1).read_to_end(&mut bytes).map_err(|_| error(ErrorCode::IoError))?;
        if bytes.len() as u64 != entry.metadata.len() {
            return Err(error(ErrorCode::DigestMismatch));
        }
        unchanged(root, &entry)?;
        Ok((parse_metadata(&bytes)?, digest_bytes(&bytes)))
    })();
    result.map_err(|mut e: GateError| {
        e.subject = Some(relative.to_owned());
        e
    })
}

pub(crate) fn read_policy(root: &Path) -> Result<Policy, GateError> {
    read_metadata(root, POLICY_PATH)
}

pub(crate) fn read_source_records(root: &Path) -> Result<SourceRecords, GateError> {
    read_metadata(root, SOURCES_PATH)
}

/// Reviews metadata only; explicitly unverified candidates are not artifact approvals.
pub(crate) fn check_repository_policy(root: &Path) -> Result<(), GateError> {
    let policy = read_policy(root)?;
    read_source_records(root)?.validate_policy(&policy)
}
