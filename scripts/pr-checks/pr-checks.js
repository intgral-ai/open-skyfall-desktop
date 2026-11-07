// Pull request checks for .github/workflows/pr-checks.yml (CONTRIBUTING.md, "Sending a change"):
//   1. duplicate: fails when a lower-numbered open PR already closes the same issue;
//   2. defaults: the template line `Changes a default: no | yes - <what>, old behaviour kept behind
//      preference <name>` must be present and well formed; `yes` earns the `changes-default` label;
//   3. picture: a PR adding to the showcase or docs/screenshots shows a picture in its description.
// The parsing is pure and tested in scripts/pr-checks/pr-checks.test.js; the functions that talk to
// GitHub take the github-script `github` client and `context`, so the tests can pass fakes.
'use strict';

const SKIP_LABEL = 'not-a-duplicate';
const DEFAULT_LABEL = 'changes-default';
// PRs opened before this predate the template line; they pass the defaults check with a notice.
const DEFAULTS_ENFORCED_FROM = Date.parse('2026-10-25T00:00:00Z');
// PRs opened before this predate the picture rule; they pass the picture check with a notice.
const PICTURE_ENFORCED_FROM = Date.parse('2026-11-02T00:00:00Z');
const PICTURE_HELP =
  'Show it in the description: drag a picture or video into the PR text so it can be seen without ' +
  'opening the changed files, and say in plain words what it is (CONTRIBUTING.md, "Sending a change").';
const BOT_LOGINS = new Set(['dependabot[bot]', 'github-actions[bot]', 'dependabot-preview[bot]']);

const { execFileSync } = require('node:child_process');
const { lstatSync, mkdtempSync, readFileSync, rmSync } = require('node:fs');
const { join } = require('node:path');
const { tmpdir } = require('node:os');

function generationBlocked(message) {
  return new Error(`Generation blocked: ${message}`);
}

function generationGit(cwd, args, options = {}) {
  const env = { ...process.env, ...options.env };
  delete env.GITHUB_TOKEN;
  delete env.GH_TOKEN;
  return execFileSync('git', args, { cwd, encoding: 'utf8', stdio: 'pipe', maxBuffer: 64 * 1024 * 1024, ...options, env });
}

function generationTree(cwd, revision) {
  return new Map(generationGit(cwd, ['ls-tree', '-r', '-z', revision]).split('\0').filter(Boolean).map((record) => {
    const tab = record.indexOf('\t');
    const [mode, type, oid] = record.slice(0, tab).split(' ');
    return [record.slice(tab + 1), { mode, type, oid }];
  }));
}

function generationSource(cwd, sourceRevision) {
  if (!/^[a-f0-9]{40}$/.test(sourceRevision) || generationGit(cwd, ['rev-parse', 'HEAD']).trim() !== sourceRevision) {
    throw generationBlocked('checkout must remain at the fixed trusted develop SHA.');
  }
  return generationTree(cwd, sourceRevision);
}

function generatedOutput(path, source) {
  return source.get(path)?.mode === '100644' && (
    ['THIRD-PARTY-LICENSES.txt', 'docs/features.md', 'ATTRIBUTION.md'].includes(path)
    || /^assets\/locales\/[^/]+\.json$/.test(path)
  );
}

function generatedPending(path) {
  return /^assets\/locales\/pending\/[^/]+\.json$/.test(path)
    || /^attribution\/pending\/(?!README\.md$)[^/]+\.md$/.test(path);
}

function validateGeneratedChange(path, before, after, source) {
  const regular = (entry) => !entry || (entry.mode === '100644' && entry.type === 'blob');
  if (!regular(before) || !regular(after)
    || !(generatedOutput(path, source) && after || generatedPending(path) && before && !after)) {
    throw generationBlocked(`non-generated path or unsafe mode: ${path}`);
  }
}

// Read-only job: serialize data only, never a patch/script or a checked-out reserved branch.
function captureGenerated({ cwd, sourceRevision }) {
  const source = generationSource(cwd, sourceRevision);
  const file = (path) => {
    const parts = path.split('/');
    for (let i = 1; i <= parts.length; i++) {
      let stat;
      try { stat = lstatSync(join(cwd, ...parts.slice(0, i))); }
      catch (error) {
        if (error.code === 'ENOENT') return null;
        throw error;
      }
      if (stat.isSymbolicLink() || (i < parts.length ? !stat.isDirectory() : !stat.isFile() || (stat.mode & 0o111))) {
        throw generationBlocked(`unsafe output mode: ${path}`);
      }
    }
    return { mode: '100644', type: 'blob' };
  };
  const paths = generationGit(cwd, ['diff', '--no-renames', '--name-only', '-z', sourceRevision]).split('\0').filter(Boolean);
  const untracked = generationGit(cwd, ['ls-files', '--others', '--exclude-standard', '-z']).split('\0').filter(Boolean);
  for (const path of [...paths, ...untracked]) validateGeneratedChange(path, source.get(path), file(path), source);
  const files = [];
  for (const [path, entry] of source) {
    if (generatedOutput(path, source)) {
      validateGeneratedChange(path, entry, file(path), source);
      files.push({ path, content: readFileSync(join(cwd, path)).toString('base64') });
    } else if (generatedPending(path) && !file(path)) {
      validateGeneratedChange(path, entry, null, source);
      files.push({ path, content: null });
    }
  }
  return { version: 1, sourceRevision, files };
}

// Writer: inspect Git objects and build an index/tree, without checking out any reserved content.
// The only executable module is from the fixed trusted SHA. Artifact bytes are blob data only.
function prepareGenerated({ cwd, sourceRevision, payload }) {
  const source = generationSource(cwd, sourceRevision);
  if (generationGit(cwd, ['status', '--porcelain']).trim()) throw generationBlocked('writer checkout is not clean.');
  if (payload?.version !== 1 || payload.sourceRevision !== sourceRevision || !Array.isArray(payload.files)) {
    throw generationBlocked('output artifact does not match the trusted source revision.');
  }
  const files = new Map();
  for (const entry of payload.files) {
    const path = entry?.path;
    if (typeof path !== 'string' || files.has(path)) throw generationBlocked('invalid or duplicate output path.');
    const after = entry.content === null ? null : { mode: '100644', type: 'blob' };
    validateGeneratedChange(path, source.get(path), after, source);
    if (entry.content !== null && (typeof entry.content !== 'string'
      || Buffer.from(entry.content, 'base64').toString('base64') !== entry.content)) {
      throw generationBlocked(`invalid blob encoding: ${path}`);
    }
    files.set(path, entry.content);
  }
  for (const path of source.keys()) {
    if (generatedOutput(path, source) && !files.has(path)) throw generationBlocked(`missing output snapshot: ${path}`);
  }
  const expectedHead = generationGit(cwd, ['for-each-ref', '--format=%(objectname)', 'refs/remotes/origin/chore/generated-files']).trim();
  let tree = generationGit(cwd, ['rev-parse', `${sourceRevision}^{tree}`]).trim();
  if (expectedHead) {
    const base = generationGit(cwd, ['merge-base', sourceRevision, expectedHead]).trim();
    const before = generationTree(cwd, base), after = generationTree(cwd, expectedHead);
    // Compare the branch's own delta, not the newer integration tree against an older branch.
    const paths = generationGit(cwd, ['diff', '--no-renames', '--name-only', '-z', base, expectedHead]).split('\0').filter(Boolean);
    for (const path of paths) validateGeneratedChange(path, before.get(path), after.get(path), source);
    try { tree = generationGit(cwd, ['merge-tree', '--write-tree', sourceRevision, expectedHead]).trim().split('\n')[0]; }
    catch { throw generationBlocked('reserved branch has a merge conflict; inspect it without rewriting history.'); }
  }
  const temp = mkdtempSync(join(tmpdir(), 'solidcraft-generated-index-'));
  try {
    const env = { GIT_INDEX_FILE: join(temp, 'index') };
    generationGit(cwd, ['read-tree', tree], { env });
    for (const [path, content] of files) {
      const oid = content === null ? '0'.repeat(40)
        : generationGit(cwd, ['hash-object', '-w', '--stdin'], { input: Buffer.from(content, 'base64') }).trim();
      generationGit(cwd, ['update-index', '-z', '--index-info'], { env, input: `${content === null ? '0' : '100644'} ${oid}\t${path}\0` });
    }
    tree = generationGit(cwd, ['write-tree'], { env }).trim();
  } finally { rmSync(temp, { recursive: true, force: true }); }
  if (tree === generationGit(cwd, ['rev-parse', `${sourceRevision}^{tree}`]).trim()) {
    return { changed: false, expectedHead, sourceRevision };
  }
  const parents = [...new Set([expectedHead, sourceRevision].filter(Boolean))].flatMap((sha) => ['-p', sha]);
  const env = {
    GIT_AUTHOR_NAME: 'github-actions[bot]', GIT_COMMITTER_NAME: 'github-actions[bot]',
    GIT_AUTHOR_EMAIL: '41898282+github-actions[bot]@users.noreply.github.com',
    GIT_COMMITTER_EMAIL: '41898282+github-actions[bot]@users.noreply.github.com',
  };
  const commit = generationGit(cwd, ['-c', 'commit.gpgsign=false', 'commit-tree', tree, ...parents], {
    env, input: 'Regenerate generated files and fold pending files\n',
  }).trim();
  return { changed: true, expectedHead, sourceRevision, commit };
}

async function pushGenerated({ github, context, cwd, expectedHead, commit, token }) {
  if (!/^[a-f0-9]{40}$/.test(commit) || !token) throw generationBlocked('missing prepared commit or repository token.');
  const { data: pulls } = await github.rest.pulls.list({
    ...context.repo, state: 'open', head: `${context.repo.owner}:chore/generated-files`,
  });
  if (pulls.length > 1 || pulls.some((p) => !p.draft || p.base.ref !== 'develop' || p.head.sha !== expectedHead)) {
    throw generationBlocked('draft status, target or expected PR head changed before push.');
  }
  let current = '';
  try { current = (await github.rest.git.getRef({ ...context.repo, ref: 'heads/chore/generated-files' })).data.object.sha; }
  catch (error) { if (error.status !== 404) throw error; }
  if (current !== expectedHead) throw generationBlocked('reserved branch head changed before push.');
  // Only this trusted, output-only step receives an ephemeral auth header. Never persist it.
  const env = {
    GIT_CONFIG_COUNT: '1', GIT_CONFIG_KEY_0: 'http.https://github.com/.extraheader',
    GIT_CONFIG_VALUE_0: `AUTHORIZATION: basic ${Buffer.from(`x-access-token:${token}`).toString('base64')}`,
  };
  try { generationGit(cwd, ['push', 'origin', `${commit}:refs/heads/chore/generated-files`], { env }); }
  catch { throw generationBlocked('cannot push the reserved branch; inspect head and token permissions. No fallback write is allowed.'); }
}

// Metadata admission only: this does not prove review, ancestry, CI or merge-method compliance.
// The required CI policy job consumes this result; an advisory bot check is not enforcement.
function branchPolicy({ baseRef, headRef, headRepo, baseRepo, eventName }) {
  const result = (ok, kind, reason) => ({ ok, kind, reason });
  if (eventName === 'merge_group') {
    const base = typeof baseRef === 'string' ? baseRef.replace(/^refs\/heads\//, '') : '';
    return result(base === 'develop' || base === 'main', 'not-pr',
      'Merge groups validate an integration or stable base and rely on their reviewed constituent PRs.');
  }
  if (eventName !== 'pull_request') {
    return result(true, 'not-pr', 'Branch admission is not applicable to a non-PR event.');
  }
  if (![baseRef, headRef, headRepo, baseRepo].every((v) => typeof v === 'string' && v.length > 0 && !/\s/.test(v))) {
    return result(false, 'task', 'PR base, source and repository identities must be present.');
  }
  const sameRepo = headRepo.toLowerCase() === baseRepo.toLowerCase();
  const release = /^release\/v(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)$/.test(headRef);
  if (headRef.startsWith('release/')) {
    const kind = baseRef === 'develop' ? 'reconciliation' : 'promotion';
    return result(release && sameRepo && (baseRef === 'main' || baseRef === 'develop'), kind,
      'Only a same-repository release/vX.Y.Z may promote to main or reconcile into develop.');
  }
  if (headRef.startsWith('hotfix/')) {
    return result(headRef.length > 'hotfix/'.length && sameRepo && baseRef === 'main', 'hotfix',
      'A nonempty same-repository hotfix targets main, then reconciles through main into develop.');
  }
  if (headRef.startsWith('sync/')) {
    return result(headRef.startsWith('sync/upstream-') && headRef.length > 'sync/upstream-'.length && sameRepo && baseRef === 'develop',
      'upstream-sync', 'A reviewed same-repository sync/upstream-* branch targets develop.');
  }
  if (headRef === 'main') {
    return result(sameRepo && baseRef === 'develop', 'reconciliation', 'Only same-repository main reconciles into develop.');
  }
  return result(baseRef === 'develop' && headRef !== 'develop' && !headRef.startsWith('gh-readonly-queue/'), 'task',
    'Task branches target develop; stable promotion uses a release or hotfix branch, not a task or queue ref.');
}

// Template comments and code blocks hold examples, not claims.
function stripNoise(body, { inlineCode = true } = {}) {
  const text = String(body || '')
    .replace(/<!--[\s\S]*?-->/g, '')
    .replace(/```[\s\S]*?```/g, '')
    .replace(/~~~[\s\S]*?~~~/g, '');
  return inlineCode ? text.replace(/`[^`\n]*`/g, '') : text;
}

// Issue numbers after a closing keyword: Fixes #1, closes: #2, Resolved owner/repo#3.
// A list ("Fixes #1, #2") counts every number; a URL to another repo does not.
function parseClosing(body, repoFullName) {
  const text = stripNoise(body);
  const found = new Set();
  const re = /\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\b:?[ \t]+((?:(?:[\w.-]+\/[\w.-]+)?#\d+(?:[ \t]*(?:,|and|&)[ \t]*)?)+)/gi;
  for (const m of text.matchAll(re)) {
    for (const ref of m[1].matchAll(/(?:([\w.-]+\/[\w.-]+))?#(\d+)/g)) {
      if (ref[1] && repoFullName && ref[1].toLowerCase() !== repoFullName.toLowerCase()) continue;
      found.add(Number(ref[2]));
    }
  }
  return [...found].sort((a, b) => a - b);
}

// -> { status: 'missing' | 'malformed' | 'no' | 'yes', preference?, detail? }
function parseDefaults(body) {
  const m = stripNoise(body, { inlineCode: false }).replace(/\*\*/g, '').match(/^[ \t]*(?:[-*][ \t]+)?Changes a default:[ \t]*(.*?)[ \t]*$/im);
  if (!m) return { status: 'missing' };
  const value = m[1].replace(/\*\*/g, '').trim();
  if (/^no\.?$/i.test(value)) return { status: 'no' };
  const yes = value.match(/^yes\b[ \t]*(?:—|–|->|→|-{1,2}|:)[ \t]*(.+)$/i);
  if (yes) {
    const pref = yes[1].match(/^(.*?)[ ,;]*\bold (?:behaviou?r|value) kept behind (?:preference|pref|setting)[ \t]+`?([\w.:/-]+)`?\.?$/i);
    if (pref && pref[1].trim() && !/^<.*>$/.test(pref[2])) {
      return { status: 'yes', detail: pref[1].trim(), preference: pref[2] };
    }
  }
  return { status: 'malformed', detail: value };
}

const DEFAULTS_HELP =
  'Add one line to the description: `Changes a default: no`, or ' +
  '`Changes a default: yes — <what>, old behaviour kept behind preference <name>`.';

function isBot(user) {
  return !!user && (user.type === 'Bot' || BOT_LOGINS.has(user.login) || /\[bot\]$/.test(user.login || ''));
}

const DOC_FILE = /(^docs\/|\.md$|^LICENSE|^\.github\/ISSUE_TEMPLATE\/|^\.github\/pull_request_template\.md$)/;
function isDocsOnly(files) {
  return files.length > 0 && files.every((f) => DOC_FILE.test(f));
}

// Open PRs with a lower number than `number` that close one of `issues`, by someone else.
// `pulls` is a list of REST pull objects.
function findDuplicates({ number, author, issues, pulls, repoFullName }) {
  const out = [];
  for (const p of pulls) {
    if (p.number >= number || p.state !== 'open') continue;
    if (p.user && p.user.login === author) continue; // stacked work by the same author
    if ((p.labels || []).some((l) => l.name === SKIP_LABEL)) continue;
    const shared = parseClosing(p.body, repoFullName).filter((n) => issues.includes(n));
    if (shared.length) out.push({ number: p.number, title: p.title, url: p.html_url, draft: !!p.draft, issues: shared });
  }
  return out.sort((a, b) => a.number - b.number);
}

function hasLabel(pull, name) {
  return (pull.labels || []).some((l) => l.name === name);
}

async function checkDuplicate({ github, context, core }) {
  const pull = context.payload.pull_request;
  const repoFullName = `${context.repo.owner}/${context.repo.repo}`;
  if (isBot(pull.user)) return { skipped: 'bot' };
  if (hasLabel(pull, SKIP_LABEL)) return { skipped: SKIP_LABEL };
  const issues = parseClosing(pull.body, repoFullName);
  if (!issues.length) return { skipped: 'no closing keyword' };
  const pulls = await github.paginate(github.rest.pulls.list, {
    ...context.repo, state: 'open', per_page: 100,
  });
  const dups = findDuplicates({ number: pull.number, author: pull.user.login, issues, pulls, repoFullName });
  if (!dups.length) return { dups };
  const lines = dups.map(
    (d) => `  #${d.number}${d.draft ? ' (draft)' : ''} ${d.title}\n    ${d.url}  (closes ${d.issues.map((n) => '#' + n).join(', ')})`,
  );
  core.setFailed(
    `A lower-numbered open pull request already closes the same issue, and the lowest number wins:\n${lines.join('\n')}\n` +
      `Close this one or help on that one. If this is not a duplicate (a different fix that happens to name the same issue), ` +
      `change "Fixes" to "Refs" or ask a maintainer to add the \`${SKIP_LABEL}\` label.`,
  );
  return { dups };
}

// A picture or video someone can see in the description: not one in a comment, a code block or the
// template's own text.
function hasPicture(body) {
  const text = stripNoise(body);
  return /!\[[^\]]*\]\([^)\s]+[^)]*\)/.test(text)
    || /<(?:img|video)\b[^>]*\bsrc=/i.test(text)
    || /https:\/\/github\.com\/user-attachments\/assets\/\S+/.test(text);
}

// Files that are about something to look at, whatever the rest of the PR does.
function needsPicture(files) {
  return files.some((f) => f === 'docs/showcase.md' || f.startsWith('docs/screenshots/'));
}

async function checkPicture({ github, context, core }) {
  const pull = context.payload.pull_request;
  if (isBot(pull.user)) return { skipped: 'bot' };
  if (!(Date.parse(pull.created_at) >= PICTURE_ENFORCED_FROM)) return { skipped: 'opened before enforcement' };
  const files = (await github.paginate(github.rest.pulls.listFiles, {
    ...context.repo, pull_number: pull.number, per_page: 100,
  })).map((f) => f.filename);
  if (!needsPicture(files)) return { skipped: 'nothing to show' };
  const shown = hasPicture(pull.body);
  if (!shown) core.setFailed(`This PR adds something to look at, but the description shows no picture. ${PICTURE_HELP}`);
  return { shown };
}

async function checkDefaults({ github, context, core }) {
  const pull = context.payload.pull_request;
  if (isBot(pull.user)) return { skipped: 'bot' };
  if (!(Date.parse(pull.created_at) >= DEFAULTS_ENFORCED_FROM)) {
    core.notice('The "Changes a default:" line is only enforced on pull requests opened on or after 2026-10-25; this one predates it. Please add the line when you next push.');
    return { skipped: 'opened before enforcement' };
  }
  const files = (await github.paginate(github.rest.pulls.listFiles, {
    ...context.repo, pull_number: pull.number, per_page: 100,
  })).map((f) => f.filename);
  if (isDocsOnly(files)) return { skipped: 'docs only' };
  const result = parseDefaults(pull.body);
  if (result.status === 'missing') {
    core.setFailed(`The description has no "Changes a default:" line. ${DEFAULTS_HELP}`);
  } else if (result.status === 'malformed') {
    core.setFailed(`Could not read "Changes a default: ${result.detail}". ${DEFAULTS_HELP}`);
  }
  return result;
}

// Separate job (it needs write access, which fork PRs don't get): best effort.
async function syncDefaultLabel({ github, context, core }) {
  const pull = context.payload.pull_request;
  const { status } = parseDefaults(pull.body);
  const has = hasLabel(pull, DEFAULT_LABEL);
  const args = { ...context.repo, issue_number: pull.number };
  try {
    if (status === 'yes' && !has) {
      await github.rest.issues.addLabels({ ...args, labels: [DEFAULT_LABEL] });
    } else if (status !== 'yes' && has) {
      await github.rest.issues.removeLabel({ ...args, name: DEFAULT_LABEL });
    }
  } catch (e) {
    // A fork PR's token is read-only: say so instead of failing the check.
    if (e.status !== 403) throw e;
    core.notice(`Could not ${status === 'yes' ? 'add' : 'remove'} the ${DEFAULT_LABEL} label (read-only token); a maintainer sets it.`);
  }
  core.info(`Defaults line: ${status}`);
}

module.exports = {
  captureGenerated, prepareGenerated, pushGenerated,
  branchPolicy,
  DEFAULTS_ENFORCED_FROM,  SKIP_LABEL, DEFAULT_LABEL, stripNoise, parseClosing, parseDefaults, isBot, isDocsOnly,
  findDuplicates, checkDuplicate, checkDefaults, syncDefaultLabel,
  PICTURE_ENFORCED_FROM, hasPicture, needsPicture, checkPicture,
};
