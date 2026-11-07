# Fork branch governance

This page defines the branch flow for this fork of SolidCraft. `develop` is the
integration branch and the intended default target for task PRs. `main` is the
stable baseline. This flow takes precedence over inherited upstream instructions
about branch targets, the merge queue and catch-up pushes. It does not change the
licensing or source policies in CONTRIBUTING.md.

The long-term default branch is `develop`. The repository still reports `main` as
its default branch until a separately authorized settings change is applied and
read back. Nothing on this page asserts that the target state is already active.

## Admission and merge methods

| Source | Target | Merge method |
|---|---|---|
| Task branch, including an external contributor's task branch | `develop` | Squash |
| Same-repository `release/vX.Y.Z` | `main` | Merge commit |
| Same-repository nonempty `hotfix/*` | `main` | Squash |
| Same-repository `main` or `release/vX.Y.Z` reconciliation | `develop` | Merge commit |
| Same-repository reviewed `sync/upstream-*` | `develop` | Merge commit |

Task work starts from `develop` and stays in one focused PR. Ordinary feature PRs
and direct `develop` PRs cannot target `main`. Release names use three decimal
version components without leading zeroes, such as `release/v1.2.3`. A branch in
an external fork does not become a release, hotfix or reconciliation branch by
using the same name. Import upstream work into a same-repository sync branch for
review instead of promoting an upstream ref directly.

`scripts/pr-checks/pr-checks.js` exports the pure metadata function
`branchPolicy({baseRef, headRef, headRepo, baseRepo, eventName})`. Its result is
`{ok, kind, reason}`. It classifies admission, not review, merge method, ancestry,
release artifacts or test evidence. The required CI `pr-policy` job must consume
this policy; the advisory duplicate, defaults and picture checks are not a
substitute. A GitHub merge group validates its `main` or `develop` base and relies
on its constituent reviewed PRs. Its synthetic queue ref is not a task branch.

## Promote and reconcile without replacing history

1. Create a temporary `release/vX.Y.Z` branch from the selected `develop` commit.
   Put release-specific fixes through normal checked PRs. Freeze the candidate
   while its full release gate runs.
2. Open the release PR to `main`. Require real, current-source native, platform,
   package, contract, runtime and provenance evidence where applicable. Missing
   required consumers block promotion; a host-only check is not release evidence.
3. Merge the release PR with a merge commit. Do not squash or rebase this promotion:
   both histories must remain ancestors of the new stable commit. Validate the
   actual promoted source before using any artifact; a pre-promotion artifact
   does not establish evidence for a different source revision.
4. Open a reconciliation PR from `main` to `develop` and use a merge commit.
   Reconciliation from the same release branch to `develop` is also permitted.
   Resolve conflicts without dropping either history or others' work.
5. Delete the temporary release branch only after promotion and reconciliation
   have landed. Never reset, replace or force-push `main` to match another branch.

For an urgent fix, branch `hotfix/*` from `main`, check the focused change and its
applicable gates, and open a PR to `main`. Squash the task-sized fix, then merge
`main` back into `develop` through a checked reconciliation PR. Do not cherry-pick
as a substitute for restoring shared ancestry.

For upstream updates, create `sync/upstream-*` from `develop`, merge the selected
upstream history, resolve conflicts and open a reviewed PR to `develop`. Use a
merge commit to keep that history. Never mirror or force-update `main` from upstream.

The initial merge queue is disabled; the observed repository state below also
shows that merge queue/auto-merge is unavailable. A necessary up-to-date update
and revalidation are permitted when the base moves. Recheck the updated source and
preserve the promotion/reconciliation ancestry. This supersedes upstream's blanket
prohibition on catch-up pushes, not the prohibition on rewriting `main`.

The inherited persistent `release` branch is not part of this flow. Release
promotion uses a temporary same-repository `release/vX.Y.Z` branch and a reviewed
PR to `main`; the branch is deleted after promotion and reconciliation. Do not
recreate or advance a long-lived `release` branch, and do not treat the inherited
Release workflow as a release trigger. Its source is now build/check-only with no
schedule, push or release-branch trigger, and the workflow is disabled manually in
the current repository. Publication remains blocked as described below.

## Generated files are reviewed changes

The Generated files workflow runs only for `develop`; select `develop` for a manual
run too. Both jobs pin their executable checkout to that event's fixed integration
SHA, with persisted checkout credentials disabled. The read-only generation job
runs Cargo without a repository token in its environment or Git configuration.
It exports a data-only JSON snapshot outside the checkout, not executable files.

A separate write job serializes updates to the short-lived, reserved
`chore/generated-files` branch and opens or updates one **draft PR to `develop`**.
It never runs Cargo or checks out that branch. It reads Git objects, validates the
branch's delta from its merge base with the trusted source, and builds only Git
blobs and a commit. New trusted integration changes are not mistaken for branch
pollution. The only permitted changes are the three generated reference files,
the exact existing top-level locale JSON files, and deletion of existing pending
locale/attribution entries (not their README). All file modes must be regular
non-executable files. Unknown paths, configuration, symlinks, executable modes,
invalid artifacts and merge conflicts block the run without discarding branch
contents. An accepted refresh replaces only those known generated outputs with
the fresh snapshot and preserves both histories as commit parents.

Only the trusted output-push step receives an ephemeral repository auth header;
it is not persisted or passed to Cargo. The workflow uses normal commits and
non-forced pushes only to the reserved branch, never a reset or force push.

The workflow never uses a deploy key, writes directly to `main` or `develop`,
skips CI in a commit message, approves its PR, marks it ready or merges it. It
checks draft status and the expected PR/branch head again immediately before its
push. An already-ready PR or a changed head blocks that push. These GitHub API
reads and the Git push are not atomic: a ready transition after the last read can
still race with the push. This is not a strict head freeze. Maintainers must
recheck the final head and its real checks before merge, and finish or close a
ready PR before regenerating. Delete the short-lived branch after its squash merge.

`GITHUB_TOKEN` writes do not reliably trigger subsequent PR CI. Creating a draft
is not a passing check. An authorized maintainer must inspect the diff, mark the
draft ready for review, and observe the real required checks for the current head
before merging. If a draft is updated again, trigger and verify fresh checks after
the final update. Never infer test success from the generation workflow's result.

If the repository token cannot push the reserved branch or create/update its PR,
generation fails visibly. A failed PR creation may leave only the reserved branch;
that is a blocked run, not permission to merge or push directly to integration.
Request the narrow repository permission separately. Do not add a secret, fall
back to another credential, or bypass the branch rules silently.

## Reviewer, check and merge policy

The owner selected **zero mandatory independent approvals** and **no administrator
bypass**. Zero required approvals does not waive checked PRs, real validation or
the human review and CI-trigger step for generated changes. Independent review
may still be requested.

The intended settings are: PR-only changes to `develop` and `main`, conversation
resolution and up-to-date checks required, no force pushes or branch deletion,
linear history off, and an empty bypass list. `develop` requires the stable `ci`
context; `main` requires both `ci` and `release-gate`. Task PRs are squash-merged.
Release promotion and `main`/`develop` reconciliation use merge commits so both
histories stay ancestors. The repository-level rebase merge switch is currently
on but this policy does not use it; a later settings change may turn it off.
Merge queue/auto-merge is unavailable in the observed state, so `merge_group`
support in the workflows is readiness for a future queue, not an activated queue.

## Observed GitHub state (read-only, 2026-11-07)

These facts were read with `gh api` and are **observations, not applied settings
and not check evidence**:

- Default branch: `main`. `main` and `develop` are both unprotected, and the
  repository ruleset list is empty; there are no effective rules.
- Repository merge settings: squash merge enabled, merge commit enabled, rebase
  merge enabled, auto-merge disabled, delete-branch-on-merge disabled. Merge queue
  is not available.
- 12 workflows are active. These 7 are disabled manually: Docker, Publish Flatpak
  repository to Pages, Generated files, Pages, Release, Snap Store and WinGet.
- PRs #18–#22 currently report no checks. An empty check list is missing evidence,
  not a pass.
- The local `native-baseline.yml` source is not part of those counts until it is
  merged and observed; it adds no successful-run evidence by existing.

Activation still needs separate owner approval, applied settings, and a read-back
of the actual rulesets/required contexts. Until then, do not describe these local
policies as enforced GitHub protection, and do not merge on an empty or skipped
check list. Keep publication disabled: this flow authorizes no release, tag,
registry, store, Pages, signing or credential setup. The #17 foundation does not
close the runtime and installed-release evidence gaps.

## Destination quarantine

This is a **repository-source inventory**, not a network probe or proof of domain,
store, registry or signing ownership. All inherited and prospective fork
publication destinations below are **unapproved and blocked**. Do not open the
app/updater, request signing, create credentials or contact a destination to
"check" it. Product defaults are deliberately unchanged by this CI foundation.

| Surface | Exact inherited identity/origin and repository evidence | Foundation disposition |
|---|---|---|
| Updater discovery | `crates/ui-egui/src/update.rs`: `REPO = tangency-labs/solidcraft`; `https://api.github.com/repos/tangency-labs/solidcraft/releases/latest`; fallback `https://github.com/tangency-labs/solidcraft/releases/latest` | Blocked for fork release acceptance; no updater launch or default rewrite |
| Updater release/asset allowlist | Same file's `RELEASE_PREFIXES`: `https://github.com/bherbruck/solidcraft/releases/` and `https://github.com/tangency-labs/solidcraft/releases/` (including download assets/checksums); release-note history uses the latter repository | Both upstream origins unapproved for this fork; no fork URL silently added |
| Update signing | `crates/ui-egui/src/update/install.rs`: `PUBLIC_KEY = None`; inherited `UPDATE_SIGNING_KEY`/`SHA256SUMS.txt.minisig` path in pre-foundation release workflow | No signing secret/read/request; unchanged product key is not a release-security approval |
| AppImage update metadata | `packaging/linux/package.sh`: `gh-releases-zsync\|OWNER\|REPO\|latest\|solidcraft-*-linux-ARCH.AppImage.zsync`, from `GITHUB_REPOSITORY`, fallback `bherbruck/solidcraft` | Build metadata only; no AppImage updater invocation/publication |
| GitHub release/download links | Inherited `docs/releasing.md`/README paths used `https://github.com/bherbruck/solidcraft/releases/latest/download/…`; updater also accepts `tangency-labs/solidcraft` | No draft or release job, no tag/asset mutation, even on manual runs |
| Windows SignPath | Pre-foundation `release.yml`: organization `fa603ff4-c702-404b-8588-a524371c7a4e`, project `solidcraft`, policies `test-signing` / `release-signing`, `SIGNPATH_API_TOKEN` / `SIGNPATH_SIGNING_POLICY`; config `packaging/windows/signpath-artifact-configuration.xml` | Signing actions, token references and policy override removed; upstream identity is blocked, including test-signing |
| Apple signing/notarization | `packaging/macos/signing.sh` and inherited release workflow used `APPLE_CERTIFICATE_P12`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_API_KEY_P8`, `APPLE_API_KEY_ID`, `APPLE_API_ISSUER` | Workflow no longer sets up a signing identity or passes credentials; only existing ad-hoc packaging remains |
| Snap | `packaging/linux/snap/snapcraft.yaml`: name `solidcraft`; inherited store steps used `latest/edge`, `latest/candidate`, `latest/stable` and `SNAPCRAFT_STORE_CREDENTIALS` | Upload removed, promotion job explicitly disabled; store/name ownership unapproved |
| WinGet | Inherited `winget.yml`: `bherbruck.SolidCraft`, submission repository `microsoft/winget-pkgs`, manifest `manifests/b/bherbruck/SolidCraft`; inherited release docs named `bherbruck/winget-pkgs` as Komac's fork and `WINGET_TOKEN` | Submission disabled with no Komac/token/write steps; no upstream store PR |
| Pages canonical/transfer addresses | `docs/book/seo.sh`: `https://docs.solidcraft.io`; `README.md`: `https://solidcraft.io`, `/download`, `/app`; `docs/compatibility.md` records transfer from `bherbruck.github.io/solidcraft` to `docs.solidcraft.io`, including `https://docs.solidcraft.io/flatpak/repo/` | No deployment/configure-pages/domain changes. Local site artifacts are not an approved fork site or transfer |
| Pages/Flatpak routes | `packaging/flatpak-site.sh`: `PAGES_BASE_URL` from configure-pages, HTTPS `/flatpak/repo/` and `solidcraft.flatpakrepo`; inherited release docs named project Pages `/solidcraft/`; old compatibility routes remain documented only in `docs/compatibility.md` | No release-triggered redispatch, remote feed assembly or Pages upload/deploy; sandbox package smoke remains separate |
| GHCR | Inherited `docker.yml`: `ghcr.io/${GITHUB_REPOSITORY_OWNER,,}/solidcraft-web`, `latest`, version/major.minor and `edge` tags. In upstream repositories this resolves to `ghcr.io/tangency-labs/solidcraft-web` or historical `ghcr.io/bherbruck/solidcraft-web`; in this fork it would resolve to `ghcr.io/intgral-ai/solidcraft-web` | All namespaces unapproved; login/push/write steps removed, build uses local `solidcraft-web:test` only |
| Homebrew | Inherited release docs name `tangency-labs/homebrew-tap`, `Casks/solidcraft.rb`, cask `tangency-labs/tap/solidcraft` | No tap mutation or implied publication authorization |

The first nonpublishing package builds do not migrate these product destinations.
A separately approved product change must settle safe fork updater/defaults and
every publication/signing identity before distribution. Disabled publisher jobs
are never required checks or accepted skipped release evidence. The exact
final-SHA rebuild and owner-consumer requirements are in [releasing.md](releasing.md).
