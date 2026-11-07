# Releasing SolidCraft in this fork

**Publication is blocked.** This foundation provides nonpublishing package builds
and a fail-closed release gate, not a release service. No tag, GitHub Release
(including a draft), store submission, Pages deployment, container push, signing
request or credential setup is authorized. Adding a secret does not enable one.
See the [destination quarantine](fork-governance.md#destination-quarantine).

## Candidate and final source

Development integrates through `develop`. Use a temporary same-repository
`release/vX.Y.Z` PR to `main`, not a persistent `release` branch. Task PRs squash;
release promotion and main/develop reconciliation use **merge commits**, preserving
both histories. Never replace, reset or force-push `main`.

Choose a workspace version from the actual user-visible changes; moving to `1.0`
needs an explicit product decision. A later authorized version-preparation change
updates `[workspace.package] version` in `Cargo.toml`, refreshes only workspace
versions with `cargo update --workspace --offline`, and folds pending formats with
`scripts/fold-format-pending.sh`. It goes through a checked PR, never `[skip ci]`.
This page does not authorize making those changes now.

Record the **final tested checkout SHA**, not just a branch, tag or version. The
release gate runs on the main-target PR merge SHA or main merge-group SHA, and can
also be called manually or by a reusable workflow. An authorized later manual run
must select the exact reviewed source. A run on another SHA cannot satisfy main's
required check. No workflow here changes branch protection/settings.

If promotion, conflict resolution or a base update changes that SHA, **rebuild all
packages and rerun all required validation at the new final SHA**. Do not relabel
old inventories, receipts or artifacts, move a tag, or claim that equal trees make
different source revisions interchangeable. Reconcile main back to develop with
a checked merge commit after promotion; clean up the temporary release branch only
after both histories are preserved.

## What runs without publishing

- `release.yml` has reusable and manual build/check entries only. There is no
  schedule, push/release trigger, `draft`/`packages_only` publication switch,
  release/store job or signing-secret interface. No nightly helper is invoked.
  Its historical selection tests remain in `packaging/tests/nightly_test.py`.
- Linux x86_64/aarch64 packages include AppImage/zsync, deb, rpm and tar.gz;
  Flatpak, Arch and Snap reuse those Linux binaries. FreeBSD, universal macOS,
  Windows x64/x86/arm64 and web packages retain their build/package checks.
  Snap and Arch failures are no longer allowed errors, including prereleases.
- Packages are unsigned, except the macOS package script's existing ad-hoc local
  signature. There are no Apple, SignPath or update-signing requests.
- Package artifacts remain Actions artifacts, never downloads published to a
  release/store/registry. Each final package artifact also contains `build.json`:
  checkout `sourceRevision`, package identity, `target`, SHA-256 of the exact #5
  contract manifest bytes (`contractDigest`, `sha256:` plus 64 lowercase hex as
  frozen by #5/#15), and names/SHA-256 of built files.
  A separate `package-evidence-*` artifact carries the same descriptor. An absent
  contract yields null, **not** a claimed digest or release-ready artifact.
  Intermediate icons/macOS binaries use SHA-qualified artifact names and only
  same-run downloads from that exact checkout; no old-run artifact lookup exists.
  Descriptor production and release aggregation refuse modified tracked source.
- `build.json` is build metadata, **not #15's runtime inventory or verification**.
  In particular `universal-apple-darwin` names the combined macOS build; it cannot
  replace #15 inventories/installed evidence for both real Darwin targets.
- `package-builds` checks every real build result. A success there means only
  nonpublishing builds/checks succeeded, not runtime/installed release acceptance.

Do not start a network-enabled desktop to smoke-test a release build. The existing
CLI/version, archive, installer and icon checks remain. Flatpak keeps its actual
desktop modelling smoke **inside its existing network-denying sandbox**, with no
network override. Snap's desktop smoke is not invoked: it shares the host network
and has no supported updater-off switch. The CLI/package checks remain; the
missing safe installed desktop/runtime acceptance is explicitly blocking, not a
skipped successful requirement. Other package desktop `--version` calls exit
before UI/updater startup; do not replace them with a normal app launch.

Docker keeps a local image build and HTTP smoke test (`push: false`). Pages keeps
the book/web build and offline browser test as an ordinary Actions artifact, with
no release-channel/Flatpak downloads or deployment. Docker/Pages/Flatpak/Snap/
WinGet publisher jobs have explicit false conditions and no publisher steps,
credentials or write permissions. Their skipped jobs are **not required checks**
and never contribute release evidence. Changing the default branch or manually
running these workflows cannot enable publication.

## Required context: `release-gate`

The top-level, always-evaluated job is named exactly `release-gate`. PRs targeting
main, main merge groups, and manual/reusable full validation select full native,
wasm, platform, heavy/property, package, contract/runtime/integration and provenance
requirements. No draft/approval label reduces those requirements. Develop merge
groups report explicit non-applicability for stable promotion; develop task PRs
continue to use their applicable `ci` gate, not a fake release success.

The aggregate checks actual reusable-workflow results, the immutable full
`releaseRequired` plan and its exact receipt digests via `scripts/ci/gate.py`,
and all 14 final package descriptors against the checkout SHA/target/contract.
The nested `ci` aggregate already checks its real selected jobs and platform/heavy
results. Missing/failed/cancelled/skipped required jobs, absent receipts,
incomplete/empty/skipped test identities, stale SHA/digests, or missing descriptors
fail. Raw unknown native failures remain blocking; the existing reviewed
known-failing grader is not a full-suite pass or publication approval.

**The release gate intentionally remains red in this foundation.** Actual #5/#7
contract/runtime consumers, #15 artifact verification/inventories and #14 installed
consumers must be integrated under their owners' approved handoffs. Even successful
native/package/rig validation cannot remove these future-consumer stops.

The existing kernel rig is a real required consumer, not a future integration
placeholder. Release calls explicitly force **every area and the ladder**, even
on draft/affected PR events. A source-bound plan selects exact existing case paths;
the existing Rust command-coverage, area known-failing and ladder-golden graders
run unchanged. Same-run SHA-qualified binary/results artifacts bind the checkout,
actual build/coverage/shard command exits, binary hashes and complete raw case
outcomes. The nested aggregate checks physical binary hashes and real job results;
the release aggregate rechecks its full raw handoff and requires that reusable
workflow's actual success. A constant pass flag, partial/empty/skipped report,
missing shard or wrong source/digest cannot stand in for this evidence.

Direct and reusable CI/rig calls have separate concurrency namespaces, including
the caller workflow name, while updates within each caller/event/ref still dedup.
The rig uses read-only GitHub-hosted Linux runners, bounded existing area shards
and build concurrency, software Vulkan and repository-scoped dependency caches
saved only by trusted main/develop pushes. These are execution requirements, not
proof of actual hosted capacity or native test success; measured runs are still
required before activation.

### Owner interfaces, not substitute implementations

#17 consumes #5's same-checkout `schema/mastra-runtime/v1/manifest.json` and bundle;
it does not create a competing schema. Verify actual registered Rust contract and
lifecycle selections before enabling owner filters. #7's later real
`packages/mastra-runtime/ci.sh` must provide locked install/typecheck/test/build,
contract and integration modes with the frozen source/contract/report arguments;
there is no executable stub here.

Only #15 implements `cargo xtask runtime-artifact`. Its approved modes are:

```text
--mode policy
--mode host-only --tree . --tree-kind repository --target TARGET --expected-revision SHA --advertised false
--mode artifact --tree STAGING --inputs-root AUDITED_INPUTS --approval APPROVAL_ID --target TARGET --expected-revision SHA --expected-contract-digest CONTRACT_DIGEST --expected-policy-digest POLICY_DIGEST --expected-sources-digest SOURCES_DIGEST --advertised true
```

These are a deferred integration contract, **not commands available or run in this
branch**. Use fixed trusted-root policy/source paths and expected digests derived
from the immutable tested checkout, not producer-selected alternatives. For each
actual staged runtime/target, require successful verifier exit **and** its matching
`artifact`/`verified` report and source/target/contract/policy/source digests. Only
that owner verifier validates the staging/resource inventory. Policy success,
`policy_valid`, `host_only`/`not_applicable`, an arbitrary JSON claiming `verified`,
or a host-only package must never stand in. There is no `if exists` success path.

#14 must supply real installed-runtime tests and source/resource manifests tied to
the same final artifacts/targets/digest. This foundation neither stages a runtime
nor installs Node nor invents an installed-consumer entry point. Until these real
owner adapters land, explicit aggregate failures remain; an uploaded file alone
does not unlock them. A changed final SHA requires new inventories and rebuilds,
not only rerunning a policy check.

## Validation and later activation

Run packaging Python suites, selector/gate tests, actionlint and shellcheck for
workflow changes. Before any implementation commit, run the repository's full
`cargo xtask ci` gate under the documented bounded-resource policy. Focused lint
or unit-test success is not native/wasm/installed validation. Preserve raw full
baseline failures separately; do not add exclusions or a broad failure allowlist.

Real GitHub-hosted cache/runner capacity, image versions, memory/disk/time and
actual per-target installed evidence need later measured runs. Local YAML/lint
success certifies none of those. Settings activation, real checks/read-back and
publication destination/signing approval are separate owner decisions; #17 stays
open for missing runtime and installed-release acceptance. No dispatch, settings,
credential or publishing action is requested by this document.

## macOS disk image implementation

`packaging/macos/package.sh` builds the `.dmg` with hash-pinned dmgbuild in a
throwaway `target/` venv, without Finder/AppleScript. The artwork/layout remains
in `assets/platforms/macos/dmg/`; this foundation changes none of it. The script
mounts the finished image and checks its layout, volume icon, bundle and local
signature, then checks the CLI zip. A future signed/notarized distribution needs
its own approval and real platform verification; an ad-hoc signature is not one.
