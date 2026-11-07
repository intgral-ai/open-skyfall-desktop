# Desktop Mastra distribution policy

**Status:** static policy and explicit CLI admission for [#15](https://github.com/intgral-ai/open-skyfall-desktop/issues/15),
implementing the scope in [the hosting contract](mastra-runtime.md#1-product-and-distribution-decisions).
No runtime, Node binary, dependency release or installed package is approved here.
`cargo xtask runtime-artifact` is a production command. `cargo xtask ci` checks
the fixed reviewed distribution metadata immediately after its existing Cargo
license check. This static CI step is not npm/Node coverage or artifact admission.

## Reviewed policy and observed artifacts

[`packaging/mastra/policy.json`](../packaging/mastra/policy.json) records the four
intended desktop targets and an exact reviewed license-expression vocabulary.
Those expressions are not dependency approvals. Its real `approvals` array is
empty: no actual inventory digest, Node pin or package evidence has been supplied.
Only a reviewed repository change may approve an exact target and inventory byte
digest. Synthetic approvals exist only in temporary, original test fixtures.

Static policy validity permits explicitly unverified source candidates. It does
not mean that any candidate, artifact or installed target is verified. Cargo's
license check does not cover Node, npm packages or their distributed files.
Artifact admission will also require exact payload/input bytes, independent
license and notice evidence, dependency closure, provenance and build identities.
There is no wildcard, environment override or regenerate-and-accept approval.

## Distribution metadata 1.0

The strict serde DTOs in [`xtask/src/runtime_artifact.rs`](../xtask/src/runtime_artifact.rs)
are the executable source of truth. The following 17 shapes document that same
wire contract, not another JSON schema, generator or validator. Field names are
case-sensitive camelCase; object key order is not significant. `T[]` means a JSON
array and `T | null` permits an explicit JSON null, not an omitted field.
**All fields are required**, including empty arrays and nullable fields.
**Missing is not null**: nine nullable field slots use required-nullable parsing
(six in SourceRecord, two in BuildInput, one in Derivation). Parsing goes directly
to typed DTOs, rejecting unknown and duplicate keys at every nested object; it
does not first collapse objects through an untyped JSON map. No defaults, version
fallback or future-field tolerance apply.

### Scalars, paths and budgets

| Type | JSON value and current executable rule |
|---|---|
| `SchemaVersion` | Exact string `"1.0"`. Every `protocolVersions` is exactly `["1.0"]`. |
| `Digest` | String `sha256:` followed by exactly 64 lowercase hexadecimal digits; SHA-256 over exact bytes, with no newline, whitespace or JSON normalization. |
| `GitRevision` | String of exactly 40 lowercase hexadecimal digits; a future Git hash format needs a schema revision. |
| `Id` | String matching `[a-z0-9][a-z0-9._-]{0,127}`. Dependency IDs are not package names. |
| `Version` | Exact SemVer core of three nonnegative decimal components, without leading zeros; optional nonempty dot-separated ASCII alphanumeric/hyphen prerelease identifiers, with no leading zeros for numeric identifiers. No build metadata, ranges, tags, `v` prefix or `latest`. |
| `Text` | JSON string whose trimmed value is nonempty; no trimming or rewriting of the stored value. Names, rights holders, license expressions, audit text and lock integrity are not IDs. |
| `HttpsUrl` | Provenance string beginning `https://`, with nonempty authority and no userinfo (`@` in authority), whitespace, control characters or backslashes. Never fetched; syntax is not proof of source existence or rights. |
| `RelPath` | Nonempty ASCII relative `/`-separated string, at most 1,024 bytes. No absolute/drive/UNC form, backslash, control/NUL, `:<>"\|?*`, empty/`.`/`..` component or component ending in dot/space. Reject Windows device stems `CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`, including extensions, case-insensitively. Never repair hostile paths. |
| `SizeBytes` | JSON unsigned integer (`u64`), 0 through 2,147,483,648 inclusive (2 GiB); no negative or fractional value. |
| `Target` | `"aarch64-apple-darwin"`, `"x86_64-apple-darwin"`, `"x86_64-pc-windows-msvc"`, `"x86_64-unknown-linux-gnu"`. Other artifact targets fail; host-only accepts the common `Id` grammar without advertising support. |
| `Role` / `ExcludedRole` | Role: `"agent-loop"`, `"model-runtime"`, `"orchestration"`. ExcludedRole: `"account"`, `"database"`, `"script-execution"`, `"server-bootstrap"`. |
| `ReviewStatus` / `PackageManagerName` | ReviewStatus: `"unverified"` or `"verified"`. PackageManagerName: `"npm"`, `"pnpm"` or `"yarn"`. |
| `OriginKind` / `OutputRole` | OriginKind: `"repository"`, `"dependency"`, `"node"`, `"tool"`, `"build-evidence"`. OutputRole: `"code"`, `"node"`, `"data"`, `"license"`, `"notice"`. |

All paths, including delivered, materialized and original upstream paths, reject
a component equal to `ee`, case-insensitively, regardless of package name.
License expressions are exact reviewed strings, not a home-grown SPDX evaluator;
unknown expressions fail admission. A package's license string is not rights
clearance. The current policy vocabulary is `Apache-2.0`, `MIT`, `MIT OR Apache-2.0`.

JSON is bounded to 16,777,216 bytes (16 MiB) per file, including BuildEvidence;
serde's recursion limit remains enabled (128). Each array has at most 100,000
entries. Declared input **plus** output sizes total at most 68,719,476,736 bytes
(64 GiB), with checked arithmetic. Each tree inspection separately bounds its
observed regular-file bytes to 64 GiB, individual files to 2 GiB and discovered
entries (including directories) to 100,000. File hashing streams through a fixed
buffer, with length and metadata rechecks; sparse files are not exempt.

Callers must keep repository, artifact and input stages **CI-owned, quiescent**
throughout evaluation. Root/ancestor/type checks, snapshots and metadata rechecks
detect declared changes but are **not adversarial filesystem isolation** or a
proof against malicious concurrent writers/TOCTOU. Symlinks and non-file/non-directory
entries are rejected; artifact/input roots have no host-only cache exclusions.

### Complete required object shapes

```text
Policy {
  schemaVersion: SchemaVersion, policyId: Id, sourceRecordsPath: RelPath,
  supportedTargets: Target[], allowedLicenseExpressions: Text[],
  forbiddenPathComponents: Text[], approvals: Approval[]
}
Approval { id: Id, target: Target, inventoryDigest: Digest }
```

`sourceRecordsPath` is exactly `packaging/mastra/source-records.json` and
`forbiddenPathComponents` exactly `["ee"]`. Supported targets and allowed
expressions are nonempty; an approval's target must be in supportedTargets.
The real policy lists exactly the four targets above, has policyId
`solidcraft-desktop-mastra-v1` and still has **approvals: []**.

```text
SourceRecords {
  schemaVersion: SchemaVersion, authorizations: Authorization[], records: SourceRecord[]
}
Authorization {
  id: Id, actor: Text, declaration: Text, proposition: Text,
  sourceRepository: HttpsUrl, sourceRevision: GitRevision,
  eligibleRoles: Role[], excludedRoles: ExcludedRole[], licenseGrant: Text, evidence: Text
}
SourceRecord {
  id: Id, authorizationId: Id, role: Role,
  sourceRepository: HttpsUrl, sourceRevision: GitRevision, sourcePath: RelPath,
  sourceDigest: Digest | null, destinationPath: RelPath | null,
  destinationDigest: Digest | null, adaptation: Text | null,
  rightsHolder: Text | null, licenseExpression: Text | null,
  requiredNotices: FileRef[], dependencyIds: Id[],
  thirdPartyReview: Review, verification: Review
}
Review { status: ReviewStatus, evidence: Text[] }
FileRef { path: RelPath, digest: Digest }
```

`requiredNotices` uses the same FileRef shape (the retained-notice reference),
with repository-relative destination paths. Authorization licenseGrant is exactly
`MIT OR Apache-2.0`; eligible roles are nonempty and excludedRoles contains all four
excluded roles above. The real owner authorization additionally fixes its actor,
declaration, proposition, repository, upstream revision, full eligible roles and
evidence to the scoped instruction below. Each record must reference a matching
authorization/repository/revision/eligible role. A verified third-party review
needs nonempty evidence. Verified per-file status additionally requires all six
nullable fields nonnull, rightsHolder `intgral-ai`, a verified third-party review
and nonempty verification evidence. Any nonnull licenseExpression must belong to
the policy vocabulary. Explicit unverified candidates are not artifact permission.

```text
Inventory {
  schemaVersion: SchemaVersion, artifactId: Id, target: Target, runtimeVersion: Version,
  protocolVersions: SchemaVersion[], node: Node, packageManager: PackageManager,
  lockfile: FileRef, contractManifest: FileRef,
  rootDependencyIds: Id[], dependencies: Dependency[], rootToolIds: Id[],
  buildTools: Dependency[], licenses: LicenseEvidence[], inputs: BuildInput[],
  outputs: Output[], derivations: Derivation[]
}
Node {
  version: Version, sourceUrl: HttpsUrl, archiveDigest: Digest, executablePath: RelPath
}
PackageManager {
  name: PackageManagerName, version: Version, sourceUrl: HttpsUrl, archiveDigest: Digest
}
Dependency {
  id: Id, name: Text, version: Version, sourceUrl: HttpsUrl,
  archiveDigest: Digest, lockIntegrity: Text, dependencyIds: Id[]
}
LicenseEvidence {
  id: Id, expression: Text, rightsHolder: Text, licenseFiles: FileRef[], noticeFiles: FileRef[]
}
BuildInput {
  id: Id, materializedPath: RelPath, originKind: OriginKind, originId: Id,
  originalPath: RelPath, sourceRevision: GitRevision | null, digest: Digest,
  sizeBytes: SizeBytes, licenseIds: Id[], sourceRecordId: Id | null
}
Output {
  path: RelPath, digest: Digest, sizeBytes: SizeBytes, role: OutputRole, licenseIds: Id[]
}
Derivation {
  outputPath: RelPath, inputIds: Id[], toolInputIds: Id[], evidenceInputId: Id | null
}
BuildEvidence {
  schemaVersion: SchemaVersion, outputPath: RelPath, inputIds: Id[], toolInputIds: Id[]
}
```

`lockfile` and `contractManifest` reference exact repository bytes. Node and
package-manager sourceUrl/archiveDigest pins are independent of the delivered
Node executable's output digest. Dependency names are separate from registry IDs;
`lockIntegrity` preserves the selected manager's exact nonblank value, not a value
inferred or regenerated by this verifier. Independent SHA-256 archive pins remain
required. `dependencies` is exactly the reachable production closure from
rootDependencyIds; `buildTools` is the separate reachable closure from rootToolIds,
using each Dependency's dependencyIds in its own registry. #7 supplies the real
lock/production classification audit; this gate does not execute a resolver.

`materializedPath` is input-root-relative; `originalPath` preserves the original
source path after renaming. A migrated input's nonnull sourceRecordId equals
originId and selects the verified SourceRecord: originalPath/sourceRevision keep
the upstream path/pin, digest matches destinationDigest, and the actual adapted
repository destination bytes must match. Otherwise repository/build-evidence
originId is the input's own ID; dependency originId is a dependencies ID; tool
originId is a buildTools ID or literal `package-manager`; Node originId is `node`.
Package-manager inputs use originKind `tool`. Original repository and generated
evidence inputs require explicit null sourceRevision; Receipt/CI alone binds their
current enclosing revision. External Git pins stay in the exact reviewed
Inventory, not replaced by that revision. Non-Git archives may use null revision,
without waiving version/archive/integrity/source evidence. Generated evidence also
requires null sourceRecordId and identical original/materialized paths.

Each input/output has nonempty resolving licenseIds; licenseFiles is nonempty.
An empty noticeFiles array records the reviewed audit's finding, not inferred
permission. License/notice FileRefs must resolve to delivered outputs with the
matching digest, role and license ID. There is exactly one Node-role output at
node.executablePath; Node is copied, not transformed. Every output has exactly one
derivation and nonempty inputIds. Copies and transformations, complete per-output
BuildEvidence sets, actual dependency owners, no orphans and retained notices
obey the detailed provenance rules below; empty global roots are not an escape
from required reachable entries. BuildEvidence's arrays equal the consuming
derivation exactly, and its outputPath matches; it is a separately hashed input,
never its own data/tool source. Policy/Receipt controls are not runtime build code.

```text
RuntimeReceipt {
  schemaVersion: SchemaVersion, runtimeVersion: Version, protocolVersions: SchemaVersion[],
  targetTriple: Target, nodeVersion: Version, repositoryRevision: GitRevision,
  contractDigest: Digest, policyDigest: Digest, sourceRecordsDigest: Digest,
  provenanceInventory: FileRef, artifactDigests: FileRef[]
}
```

`provenanceInventory.path` is exactly `inventory.json`. Receipt identities match
Inventory and independent trusted CLI expectations; repositoryRevision is the
current-build authority, not a substitute for upstream pins. artifactDigests is
exactly the Inventory output path/digest map. Neither control hashes itself;
inventory.json/runtime.json remain strict controls, not hidden payload or a way to
approve producer observations. Artifact files equal outputs plus these two
controls; input files equal all materializedPath entries.

### Array ordering and uniqueness

All set arrays below are strictly ascending and duplicate-free, not build order.
Strings/IDs use Rust lexical ordering; enum orders here coincide with the listed
wire-string lexical order. JSON object field order is unrestricted.

| Arrays | Required ordering/key |
|---|---|
| Policy supportedTargets / allowedLicenseExpressions | Target / exact Text value. forbiddenPathComponents is the exact singleton `["ee"]`. |
| Policy approvals; SourceRecords authorizations / records | `id`. |
| Authorization eligibleRoles / excludedRoles; Review evidence | Role / ExcludedRole / exact Text value. |
| SourceRecord dependencyIds; Inventory rootDependencyIds / rootToolIds; Dependency dependencyIds; BuildInput / Output licenseIds; Derivation / BuildEvidence inputIds / toolInputIds | `Id`. |
| Inventory dependencies / buildTools / licenses / inputs | `id`; IDs are also disjoint across these four Inventory registries. |
| SourceRecord requiredNotices; LicenseEvidence licenseFiles / noticeFiles; Inventory outputs / derivations; RuntimeReceipt artifactDigests | `path` (derivations: `outputPath`), also unique under ASCII casefold. |
| Inventory / RuntimeReceipt protocolVersions | Exact singleton `["1.0"]`, not a negotiation/preference list. |

SourceRecord sourcePath values and nonnull destinationPath values are each unique
under ASCII casefold, but records remain ordered by ID, not path. Likewise input
materializedPath values are casefold-unique while inputs remain ordered by ID.
Original source paths may be referenced by multiple inputs; shared exact notice
or repository file references do not create a second file. Tree collection and
sparse repository reference checks also reject case-only leaf/parent aliases;
shared exact paths are not an excuse to accept a differently cased spelling.
Shape parsing alone does not establish resolved graph references, approved
licenses, rights or exact bytes; artifact admission still performs those checks.

## Scoped owner instruction

The owner declared **“有权代表并授权”**. The recorded actor is **Product owner
representing intgral-ai**. The exact authorized proposition is:

> The owner has the right to represent intgral-ai and authorizes adapting and redistributing in the PUBLIC intgral-ai/open-skyfall-desktop repository, under MIT OR Apache-2.0, intgral-ai-owned Vector Studio orchestration/agent-loop/model-runtime modules selected in contract section 1.5 at acb5048120b53ae1b4502724b70d1283e8c9daf4. This excludes contract-excluded account/database/server-bootstrap/script-execution code. Third-party code keeps its applicable licenses and requires independent audit; this does not attest that every file is original or authorize arbitrary source.

Evidence: **intgral-ai is the rights holder and authorizing entity. It authorized
public distribution of the adapted Vector Studio source files on 2026-11-07, under
this per-file rights record and not a repository-wide license: no root license was
identified in the private source at
`acb5048120b53ae1b4502724b70d1283e8c9daf4`. Imported files retain their applicable
notices; each file's license, notices and transitive dependencies are recorded by
the per-file audit, with transitive dependencies inventoried when #7 supplies the
artifact. This authorization covers neither npm/Node packages nor full runtime
coverage. No public permalink or named personal signatory was supplied.** This
records a relayed instruction, not a fabricated signature, named personal identity,
public attestation or per-file finding. Ownership authorization has already been
given; independent third-party license review remains necessary.

The license status is recorded honestly: no root license was identified in the
private source at the pinned revision. This is a per-file rights-holder
authorization, not a repository-wide license and not a claim that the imported
files carry `MIT OR Apache-2.0`. Imported files keep their applicable notices;
new original code may use this project's dual license. `requiredNotices` and
`dependencyIds` stay empty here only because the per-file audit has not run; that
is not a finding that no notices or dependencies exist. The audit fills the
applicable license expression, retained notices and transitive dependency closure,
and #7 inventories the dependencies when it supplies the artifact.

[`packaging/mastra/source-records.json`](../packaging/mastra/source-records.json)
binds that instruction to `https://github.com/intgral-ai/intgral-vector-studio`
at `acb5048120b53ae1b4502724b70d1283e8c9daf4`. Eligible roles are `orchestration`,
`agent-loop` and `model-runtime`; excluded roles are `account`, `database`,
`server-bootstrap` and `script-execution`. A matching role is not proof that a
particular file belongs to that role or is entirely intgral-ai-owned: the per-file
audit must establish both. Third-party portions retain their own applicable
licenses and notices, including dependencies embedded in otherwise owned files.
Enterprise `ee/` input is excluded in every path, case-insensitively; a subprocess
or private npm package does not change those rules.

## Candidate records, not imported source

Only these exact selected paths from contract section 1.5 are recorded, relative to
the fixed upstream revision:

| Record | Role | Upstream path |
|---|---|---|
| `agent-loop` | `agent-loop` | `apps/server/src/mastra/agent-loop/agent-loop.ts` |
| `cad-generation` | `orchestration` | `packages/ai-runtime/src/workflows/cad-generation.ts` |
| `model-runtime` | `model-runtime` | `packages/ai-runtime/src/models/model-runtime.ts` |
| `product-generation-runtime` | `orchestration` | `apps/server/src/mastra/services/product-generation-runtime.ts` |
| `query-proposal` | `orchestration` | `apps/server/src/mastra/workflows/query-proposal.ts` |

`packages/ai-runtime/src/tools/cad-execution.ts` is deliberately not recorded:
contract section 1.2 places the script executor outside the selected migration,
and `script-execution` is an excluded role. Section 1.5 uses only the workflow /
injected-executor separation as a reference. `product-generation-runtime.ts` is
recorded only as the selected session-orchestration candidate; its product
persistence, account/quota and server-bootstrap portions remain excluded and must
not be imported. A matching role is not proof of ownership or scope.

All five review statuses remain `unverified`. Source/destination digests, destination
paths, adaptations, rights holders and applicable license expressions are
explicitly null; notices, dependencies and audit evidence are empty. No private
source was inspected or copied to fill these gaps. Additional associated modules
need exact records, not directory-wide permission.

Before copying or adapting a file, record its exact source path/revision/digest,
destination path/digest, adaptation, applicable scoped authorization, verified
rights holder, allowed license, retained notices and dependencies. Both per-file
verification and independent third-party review need nonempty audit evidence.
Source and adapted bytes may differ, so their digests remain separate. A used
missing/unverified record fails with `SOURCE_RIGHTS_UNVERIFIED`; changing a status
alone, repository, revision or role cannot supply the missing rights. A used
record's license/notices and dependencies must be represented independently in
the inventory. Static references do not establish matching notice or code bytes;
artifact checks must verify them and trace retained notices into delivered output.
Add real pending attribution only when actual source is admitted; attribution
is not permission, and this preparation adds no placeholder attribution.

## Build revision without circular approval

The required Inventory `sourceRevision` field stays explicitly null for original
current-repository inputs and generated build evidence. Receipt/CI binds the
current repository revision externally, together with same-revision checkout and
actual file digests. A migrated input with `sourceRecordId`, even when its origin
is `repository`, retains the verified upstream revision above; external sources
are not relabeled with the current repository SHA. Task4/5 implement these checks.
This does not remove fields or exempt output bytes, rights, dependencies, notices
or provenance, and Policy/Receipt control anchors must not create self-approval.

## Exact inputs and build-evidence projection

Artifact files must equal the declared outputs plus the two strict control objects
`inventory.json` and `runtime.json`. Input-root files must equal the materialized
inputs. Every actual file has its exact byte digest and size checked; neither
control objects nor generated evidence may hide payload. Policy is a trusted
repository control input, not build code, and cannot approve itself through an
input/output derivation. Removing dependencies, files or digest fields is not an
anti-self-reference correction.

Every output has exactly one derivation. A copied file has one data input, no
tools or evidence, and identical bytes, size and licenses. A transformed output
requires data inputs, separately inventoried tool inputs and a hashed generated
evidence input. The evidence is strict JSON, with all four fields required:

```json
{
  "schemaVersion": "1.0",
  "outputPath": "dist/bundle.js",
  "inputIds": ["source-input"],
  "toolInputIds": ["compiler-input"]
}
```

`outputPath` is the exact declared relative output path; the two sorted, unique ID
arrays must equal the derivation's complete data/tool sets, not subsets or
supersets. Unknown, duplicate or missing fields are rejected. The evidence input
is referenced only as `evidenceInputId`, never as its own source/tool or delivered
code. It keeps its own digest, size, licenses, self `originId`, equal original and
materialized paths, explicit null `sourceRevision` and null `sourceRecordId`.
Receipt/CI binds its current-build revision just as for original repository code.

All input/owner references must resolve. Production dependencies and build tools
are separate exact reachable closures, with no orphan entries. Each transformed
output must cover the reachable dependency/tool owners of its selected inputs;
another output's input usage cannot fill a hole in that bundle's evidence. Graph
traversal uses bounded visited sets, so closed dependency cycles terminate and
pass only with complete coverage. A cycle does not excuse a dangling reference or
self-evidencing derivation. Build-only tools are not counted as shipped production
dependencies. Inventory/source metadata, identity and license/notice checks precede
code hashing; evidence content is parsed after the approved input bytes match.

Every used migrated source record, including an exactly copied file, additionally
requires its entire declared dependency reachable closure to have actual
materialized dependency inputs. Record IDs or dependency metadata alone are not
input bytes. Those inputs need their own legitimate output derivations and exact
hash/size checks; a missing, misowned or dangling dependency is not waived by the
source's verified rights. A copied file still has exactly one input: separate
dependency outputs do not get folded into its single-input copy derivation.

The original upstream path is retained even after copying, bundling or minifying.
An `ee` component in delivered, materialized or original paths is an absolute,
case-insensitive rejection before code content is interpreted. Renaming to
`bundle.js`, rehashing the inventory or even explicitly approving that prohibited
record cannot override it. Deleting the input record instead leaves incomplete
provenance. Non-enterprise mutations still need independent exact-byte approval.
Migrated files must match verified upstream records and actual adapted bytes;
the owner grant cannot replace third-party rights or required delivered notices.

This JSON is an interchange projection of **#7's real build metafile**, not proof
that a build was run or honestly reported. #7 must supply independently reviewed
source-to-materialized mappings, applicable rights/notices, the selected lock and
production closure, and complete real build evidence before release. These tests
execute no Node, tool or package manager. Synthetic admission and Cargo checks do
not establish real source audit, reproducibility, installed-package acceptance or
trusted checkout identity; #17/#14 retain those release obligations. Real approvals
remain empty and all five selected real source records remain unverified.

## CLI contract and #17 integration

The reviewed controls are always `packaging/mastra/policy.json` and
`packaging/mastra/source-records.json`, relative to the xtask repository root.
Producers cannot supply alternate policy/source paths or use an environment
bypass. No mode, tree, target, revision or advertised state is implicit. Unknown,
repeated and mode-incompatible flags fail with `INVALID_ARGUMENT`; missing
artifact flags never trigger a source-only fallback. These are the exact commands
#17 may wire:

```sh
# Static reviewed metadata only, including explicitly unverified candidates.
cargo xtask runtime-artifact --mode policy

# Foundation/source-only applicability, NOT installed/package evidence.
# Obtain SOURCE_SHA separately from the trusted, clean checked-out source.
cargo xtask runtime-artifact --mode host-only --tree . --tree-kind repository --target x86_64-unknown-linux-gnu --expected-revision "$SOURCE_SHA" --advertised false

# Real package payload; never substitute host-only if this request is incomplete.
cargo xtask runtime-artifact --mode artifact --tree "$MASTRA_ROOT" --inputs-root "$AUDITED_INPUT_ROOT" --approval "$APPROVAL_ID" --target "$TARGET" --expected-revision "$SOURCE_SHA" --expected-contract-digest "$CONTRACT_DIGEST" --expected-policy-digest "$POLICY_DIGEST" --expected-sources-digest "$SOURCES_DIGEST" --advertised true
```

Policy mode accepts only `--mode policy`. Host-only requires all six shown flags,
an existing tree resolving to the reviewed repository root, `tree-kind repository`
and `advertised false`; it rejects artifact flags and package trees. It skips
`.git`, top-level names beginning with `target`, and source dependency
`node_modules` directories. It still inspects `packages/`, `dist/` and other
observed directories, refusing artifact receipts, runtime-package dist payload,
Node executables named `node`/`node.exe`, and staged Mastra resources. This is a
bounded source-only applicability result, not an assertion that arbitrarily
disguised runtime bytes or excluded build caches were identified.

Artifact mode requires every shown flag, including `advertised true`, and rejects
`--tree-kind` (its package kind is derived). Its exact payload/input inspections
have no repository/cache exclusions. Unsupported targets, absent payload/input
trees, missing approvals, missing or mismatched trusted anchors, incomplete
provenance, and unverified rights fail; none becomes `not_applicable`. No command
generates, approves or executes a runtime, Node binary or package.

Each invocation writes exactly one compact JSON line on stdout on success or
failure, unless stdout itself fails. The existing main returns exit 1 on failure;
stderr contains a short fixed error code (Cargo may also write diagnostics).
An output write/flush error is `IO_ERROR`, not command success. Success has exactly
these fields, including explicit nulls and nonnegative integer counts:

```json
{"schemaVersion":"1.0","mode":"policy","status":"policy_valid","target":null,"treeKind":null,"policyDigest":"sha256:…","sourceRecordsDigest":"sha256:…","inventoryDigest":null,"filesChecked":0,"inputsChecked":0,"dependenciesChecked":0}
```

Wire modes are `policy`, `host_only`, `artifact` (the CLI uses `host-only`).
Host-only has status `not_applicable`, `treeKind:"repository"`, a target, a null
inventory digest and its actual inspected file count; input/dependency counts
are zero. Artifact success has status `verified`, `treeKind:"package"`, the target
and inventory digest, and actual checked file/input counts. `dependenciesChecked`
counts only production dependencies, not build tools. Policy counts are all zero.
There is no host-only/package `not_applicable` result. Failures have exactly:

```json
{"schemaVersion":"1.0","mode":"artifact","status":"failed","error":{"code":"INVALID_ARGUMENT","subject":null}}
```

Failure mode is null only if no valid unique mode flag can be identified; repeated
mode flags are ambiguous even if their values match. Error codes are the fixed
SCREAMING_SNAKE_CASE vocabulary; subjects are safe relative paths/IDs or null,
never parser snippets, source contents or absolute locations. The first failing
phase wins: CLI, reviewed policy/sources, requested-mode requirements, path/type
safety, enterprise paths, approval/identity, declared license/source-rights/
provenance semantics, then actual file/input hashes and closure.

#17 must obtain expected contract/policy/source digests from the exact reviewed
files at its independently trusted source SHA, not from inventory/receipt text.
Receipt binds that revision externally: current-repository/generated Inventory
inputs keep their **required null** `sourceRevision`; migrated inputs keep their
verified **upstream pin**, not the current SHA. Actual bytes, materialized
dependency closure, per-output data/tool evidence, third-party rights and retained
notices remain mandatory; this CLI does not weaken those checks.

#17 also owns clean, immutable, same-origin/same-SHA checkout and job selection.
Running host-only locally with `git rev-parse HEAD` does **not** prove this dirty
development tree is an immutable build, and `expected-revision` alone is not a
Git attestation. #17 must check JSON status **and** exit code and retain mode and
treeKind. A runtime-required job accepts only `artifact`/`verified`; neither
`policy_valid` nor `host_only`/`not_applicable` can satisfy it. Unsupported/no-runtime
package non-inclusion belongs to #14's physical package evidence, not a fake
foundation package pass. Real source/build audits (#7), trusted Windows execution,
installed-package/signature checks (#14), and release CI remain separate duties.

The existing real CLI regression tests build the production xtask entrypoint with
the ci profile even when the outer tests use the default dev profile. Their helper
passes an explicit nested target directory beside the current test profile, so an
outer custom `--target-dir` is retained without relying on CARGO_TARGET_DIR. It
executes only the exact xtask-bin executable returned by that successful Cargo
build for the current manifest, never an inferred existing/stale profile path.
