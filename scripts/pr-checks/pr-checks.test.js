'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const pc = require('./pr-checks.js');
const { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } = require('node:fs');
const { join, resolve } = require('node:path');
const { tmpdir } = require('node:os');
const { execFileSync } = require('node:child_process');

const REPO = 'tangency-labs/solidcraft';

test('parseClosing finds closing keywords only', () => {
  assert.deepEqual(pc.parseClosing('Fixes #12', REPO), [12]);
  assert.deepEqual(pc.parseClosing('closes: #3\nResolved #4', REPO), [3, 4]);
  assert.deepEqual(pc.parseClosing('Fixes #5, #6 and #7', REPO), [5, 6, 7]);
  assert.deepEqual(pc.parseClosing('Fix #8', REPO), [8]);
  assert.deepEqual(pc.parseClosing('Refs #9\nSee #10\nrelated to #11', REPO), []);
  assert.deepEqual(pc.parseClosing('Prefixes #1 and suffix fixes#2', REPO), []);
  assert.deepEqual(pc.parseClosing(null, REPO), []);
});

test('parseClosing handles repo-qualified refs, comments and code', () => {
  assert.deepEqual(pc.parseClosing(`Fixes ${REPO}#20`, REPO), [20]);
  assert.deepEqual(pc.parseClosing('Fixes other/repo#21', REPO), []);
  assert.deepEqual(pc.parseClosing('<!-- Fixes #22 -->', REPO), []);
  assert.deepEqual(pc.parseClosing('```\nFixes #23\n```\nFixes #24', REPO), [24]);
  assert.deepEqual(pc.parseClosing('say `Fixes #25` to close', REPO), []);
});

test('parseDefaults', () => {
  assert.equal(pc.parseDefaults('x').status, 'missing');
  assert.equal(pc.parseDefaults('<!-- Changes a default: no -->').status, 'missing');
  assert.equal(pc.parseDefaults('Changes a default: no').status, 'no');
  assert.equal(pc.parseDefaults('- Changes a default: No.\r\nmore').status, 'no');
  assert.equal(pc.parseDefaults('**Changes a default:** no').status, 'no');
  assert.equal(pc.parseDefaults('Changes a default: <no | yes — <what>, old behaviour kept behind preference <name>>').status, 'malformed');
  assert.equal(pc.parseDefaults('Changes a default:').status, 'malformed');
  assert.equal(pc.parseDefaults('Changes a default: maybe').status, 'malformed');
  assert.equal(pc.parseDefaults('Changes a default: yes').status, 'malformed');
  assert.equal(pc.parseDefaults('Changes a default: yes - faster zoom').status, 'malformed');
});

test('parseDefaults yes forms', () => {
  for (const sep of ['—', '–', '-', '->', '→', ':']) {
    const r = pc.parseDefaults(`Changes a default: yes ${sep} zoom speed doubled, old behaviour kept behind preference viewport.zoom_speed`);
    assert.equal(r.status, 'yes', sep);
    assert.equal(r.preference, 'viewport.zoom_speed');
    assert.equal(r.detail, 'zoom speed doubled');
  }
  assert.equal(pc.parseDefaults('Changes a default: yes — x, old value kept behind preference `p`').status, 'yes');
  assert.equal(pc.parseDefaults('Changes a default: yes — x, old behaviour kept behind preference <name>').status, 'malformed');
});

test('isBot and isDocsOnly', () => {
  assert.ok(pc.isBot({ login: 'dependabot[bot]', type: 'Bot' }));
  assert.ok(pc.isBot({ login: 'solidcraft-generated[bot]' }));
  assert.ok(!pc.isBot({ login: 'alice', type: 'User' }));
  assert.ok(pc.isDocsOnly(['README.md', 'docs/a.png']));
  assert.ok(!pc.isDocsOnly(['README.md', 'crates/a.rs']));
  assert.ok(!pc.isDocsOnly([]));
});

const mk = (number, login, body, extra = {}) => ({
  number, state: 'open', user: { login }, body, title: `t${number}`,
  html_url: `https://github.com/${REPO}/pull/${number}`, labels: [], ...extra,
});

test('findDuplicates: lower number, other author, same issue', () => {
  const pulls = [
    mk(10, 'bob', 'Fixes #5'),
    mk(11, 'carol', 'Fixes #6'),
    mk(12, 'alice', 'Fixes #5'),
    mk(13, 'dan', 'Refs #5'),
    mk(14, 'erin', 'Closes #5', { draft: true }),
    mk(15, 'fay', 'Fixes #5', { labels: [{ name: 'not-a-duplicate' }] }),
    mk(30, 'gus', 'Fixes #5'),
    mk(16, 'hal', 'Fixes #5', { state: 'closed' }),
  ];
  const d = pc.findDuplicates({ number: 20, author: 'alice', issues: [5], pulls, repoFullName: REPO });
  assert.deepEqual(d.map((x) => x.number), [10, 14]);
  assert.ok(d[1].draft);
});

function fakes(pull, { pulls = [], files = [] } = {}) {
  const calls = [];
  const github = {
    paginate: async (fn) => (fn === 'list' ? pulls : files),
    rest: {
      pulls: { list: 'list', listFiles: 'files' },
      issues: {
        addLabels: async (a) => calls.push(['add', a.labels]),
        removeLabel: async (a) => calls.push(['remove', a.name]),
      },
    },
  };
  const failures = [];
  const core = { setFailed: (m) => failures.push(m), info() {}, notice() {} };
  const context = { repo: { owner: 'tangency-labs', repo: 'solidcraft' }, payload: { pull_request: pull } };
  return { github, core, context, calls, failures };
}

test('checkDuplicate fails with links, skips on label and bots', async () => {
  const me = mk(20, 'alice', 'Fixes #5');
  let f = fakes(me, { pulls: [mk(10, 'bob', 'Fixes #5')] });
  await pc.checkDuplicate(f);
  assert.equal(f.failures.length, 1);
  assert.match(f.failures[0], /pull\/10/);

  f = fakes({ ...me, labels: [{ name: 'not-a-duplicate' }] }, { pulls: [mk(10, 'bob', 'Fixes #5')] });
  await pc.checkDuplicate(f);
  assert.equal(f.failures.length, 0);

  f = fakes({ ...me, user: { login: 'dependabot[bot]', type: 'Bot' } }, { pulls: [mk(10, 'bob', 'Fixes #5')] });
  await pc.checkDuplicate(f);
  assert.equal(f.failures.length, 0);
});

test('checkDefaults fails when missing, passes docs-only and bots', async () => {
  const me = mk(20, 'alice', 'Fixes #5', { created_at: '2026-10-25T00:00:00Z' });
  let f = fakes(me, { files: [{ filename: 'crates/a.rs' }] });
  await pc.checkDefaults(f);
  assert.equal(f.failures.length, 1);

  f = fakes(me, { files: [{ filename: 'docs/a.md' }] });
  await pc.checkDefaults(f);
  assert.equal(f.failures.length, 0);

  f = fakes({ ...me, body: 'Changes a default: no' }, { files: [{ filename: 'crates/a.rs' }] });
  await pc.checkDefaults(f);
  assert.equal(f.failures.length, 0);
});

test('syncDefaultLabel adds and removes', async () => {
  const yes = 'Changes a default: yes — a, old behaviour kept behind preference p';
  let f = fakes(mk(1, 'a', yes));
  await pc.syncDefaultLabel(f);
  assert.deepEqual(f.calls, [['add', ['changes-default']]]);
  f = fakes(mk(1, 'a', 'Changes a default: no', { labels: [{ name: 'changes-default' }] }));
  await pc.syncDefaultLabel(f);
  assert.deepEqual(f.calls, [['remove', 'changes-default']]);
});

test('syncDefaultLabel notes a read-only token instead of failing', async () => {
  const yes = 'Changes a default: yes — a, old behaviour kept behind preference p';
  const f = fakes(mk(1, 'a', yes));
  const notes = [];
  f.core.notice = (m) => notes.push(m);
  f.github.rest.issues.addLabels = async () => { throw Object.assign(new Error('Resource not accessible by integration'), { status: 403 }); };
  await pc.syncDefaultLabel(f);
  assert.equal(notes.length, 1);
  f.github.rest.issues.addLabels = async () => { throw Object.assign(new Error('server error'), { status: 500 }); };
  await assert.rejects(pc.syncDefaultLabel(f));
});

test('checkDefaults only enforces from 2026-10-25', async () => {
  const files = [{ filename: 'crates/a.rs' }];
  const old = mk(20, 'alice', 'Fixes #5', { created_at: '2026-10-24T23:59:59Z' });
  let f = fakes(old, { files });
  assert.deepEqual(await pc.checkDefaults(f), { skipped: 'opened before enforcement' });
  assert.equal(f.failures.length, 0);
  f = fakes({ ...old, created_at: '2026-10-25T00:00:00Z' }, { files });
  await pc.checkDefaults(f);
  assert.equal(f.failures.length, 1);
});

test('hasPicture needs one a reader can see in the description', () => {
  assert.equal(pc.hasPicture('![tray](https://example.com/a.gif)'), true);
  assert.equal(pc.hasPicture('<img width="960" alt="x" src="https://github.com/user-attachments/assets/abc" />'), true);
  assert.equal(pc.hasPicture('<video src="https://example.com/a.mp4"></video>'), true);
  assert.equal(pc.hasPicture('https://github.com/user-attachments/assets/abc'), true);
  assert.equal(pc.hasPicture(''), false);
  assert.equal(pc.hasPicture(null), false);
  assert.equal(pc.hasPicture('The GIF is the change.'), false);
  assert.equal(pc.hasPicture('See docs/screenshots/box.gif'), false);
  assert.equal(pc.hasPicture('<!-- ![old](https://example.com/a.gif) -->'), false);
  assert.equal(pc.hasPicture('```\n![x](https://example.com/a.gif)\n```'), false);
  assert.equal(pc.hasPicture('`![x](https://example.com/a.gif)`'), false);
});

test('needsPicture covers the showcase and its media only', () => {
  assert.equal(pc.needsPicture(['docs/showcase.md']), true);
  assert.equal(pc.needsPicture(['crates/a.rs', 'docs/screenshots/a.gif']), true);
  assert.equal(pc.needsPicture(['docs/mcp.md', 'crates/a.rs']), false);
});

test('checkPicture fails a showcase PR with no picture, from 2026-11-02 on', async () => {
  const files = [{ filename: 'docs/showcase.md' }];
  const pr = mk(30, 'alice', 'The GIF is the change.', { created_at: '2026-11-02T00:00:00Z' });
  let f = fakes(pr, { files });
  assert.deepEqual(await pc.checkPicture(f), { shown: false });
  assert.equal(f.failures.length, 1);
  f = fakes({ ...pr, body: '![it](https://example.com/a.gif)' }, { files });
  assert.deepEqual(await pc.checkPicture(f), { shown: true });
  assert.equal(f.failures.length, 0);
  f = fakes({ ...pr, created_at: '2026-11-01T23:59:59Z' }, { files });
  assert.deepEqual(await pc.checkPicture(f), { skipped: 'opened before enforcement' });
  f = fakes(pr, { files: [{ filename: 'crates/a.rs' }] });
  assert.deepEqual(await pc.checkPicture(f), { skipped: 'nothing to show' });
  assert.equal(f.failures.length, 0);
});

// These cases catch wrong routing and same-name fork branches authorizing stable promotion.
const FORK_REPO = 'intgral-ai/open-skyfall-desktop';
const admissionCases = [
  ['task to integration', 'develop', 'feat/example', FORK_REPO, true, 'task'],
  ['external task to integration', 'develop', 'fix/example', 'contributor/fork', true, 'task'],
  ['generated task to integration', 'develop', 'chore/generated-files', FORK_REPO, true, 'task'],
  ['task cannot target stable', 'main', 'feat/example', FORK_REPO, false, 'task'],
  ['integration cannot directly target stable', 'main', 'develop', FORK_REPO, false, 'task'],
  ['versioned promotion', 'main', 'release/v1.2.3', FORK_REPO, true, 'promotion'],
  ['zero version components', 'main', 'release/v0.0.0', FORK_REPO, true, 'promotion'],
  ['case-insensitive repository identity', 'main', 'release/v1.2.3', FORK_REPO.toUpperCase(), true, 'promotion'],
  ['external promotion lookalike', 'main', 'release/v1.2.3', 'contributor/fork', false, 'promotion'],
  ['hotfix to stable', 'main', 'hotfix/crash', FORK_REPO, true, 'hotfix'],
  ['external hotfix lookalike', 'main', 'hotfix/crash', 'contributor/fork', false, 'hotfix'],
  ['stable reconciliation', 'develop', 'main', FORK_REPO, true, 'reconciliation'],
  ['release reconciliation', 'develop', 'release/v1.2.3', FORK_REPO, true, 'reconciliation'],
  ['external stable lookalike', 'develop', 'main', 'contributor/fork', false, 'reconciliation'],
  ['external release reconciliation lookalike', 'develop', 'release/v1.2.3', 'contributor/fork', false, 'reconciliation'],
  ['reviewed upstream sync', 'develop', 'sync/upstream-2026-11-06', FORK_REPO, true, 'upstream-sync'],
  ['external sync lookalike', 'develop', 'sync/upstream-example', 'contributor/fork', false, 'upstream-sync'],
  ['upstream sync cannot target stable', 'main', 'sync/upstream-example', FORK_REPO, false, 'upstream-sync'],
  ['hotfix reconciles through stable', 'develop', 'hotfix/crash', FORK_REPO, false, 'hotfix'],
  ['integration cannot target itself', 'develop', 'develop', FORK_REPO, false, 'task'],
  ['unknown base', 'release/v1.2.3', 'feat/example', FORK_REPO, false, 'task'],
];
for (const [name, baseRef, headRef, headRepo, ok, kind] of admissionCases) {
  test(`branchPolicy: ${name}`, () => {
    assert.equal(typeof pc.branchPolicy, 'function', 'pure branch admission is exported');
    const input = Object.freeze({ baseRef, headRef, headRepo, baseRepo: FORK_REPO, eventName: 'pull_request' });
    const result = pc.branchPolicy(input);
    assert.equal(result.ok, ok);
    assert.equal(result.kind, kind);
    assert.equal(typeof result.reason, 'string');
    assert.ok(result.reason.length > 0);
    assert.deepEqual(pc.branchPolicy(input), result, 'metadata policy is deterministic');
  });
}

for (const headRef of ['release/v1.2', 'release/1.2.3', 'release/v01.2.3', 'release/v1.2.3-rc.1', 'release/v1.2.3/extra', 'hotfix/', 'sync/upstream-']) {
  test(`branchPolicy: malformed reserved source ${headRef} cannot become a task`, () => {
    assert.equal(typeof pc.branchPolicy, 'function');
    for (const baseRef of ['main', 'develop']) {
      assert.equal(pc.branchPolicy({ baseRef, headRef, headRepo: FORK_REPO, baseRepo: FORK_REPO, eventName: 'pull_request' }).ok, false);
    }
  });
}

test('branchPolicy rejects missing PR identity and queue refs disguised as task sources', () => {
  assert.equal(typeof pc.branchPolicy, 'function');
  const pr = { baseRef: 'develop', headRef: 'feat/example', headRepo: FORK_REPO, baseRepo: FORK_REPO, eventName: 'pull_request' };
  for (const field of ['baseRef', 'headRef', 'headRepo', 'baseRepo']) {
    assert.equal(pc.branchPolicy({ ...pr, [field]: undefined }).ok, false, field);
  }
  assert.equal(pc.branchPolicy({ ...pr, headRef: 'gh-readonly-queue/develop/pr-1-abcd' }).ok, false);
});

test('branchPolicy validates merge-group bases without treating queue refs as task branches', () => {
  assert.equal(typeof pc.branchPolicy, 'function');
  for (const baseRef of ['develop', 'main', 'refs/heads/develop', 'refs/heads/main']) {
    const result = pc.branchPolicy({ eventName: 'merge_group', baseRef, headRef: 'gh-readonly-queue/main/pr-1-abcd' });
    assert.equal(result.ok, true, baseRef);
    assert.equal(result.kind, 'not-pr');
  }
  for (const baseRef of ['', undefined, 'refs/heads/other']) {
    assert.equal(pc.branchPolicy({ eventName: 'merge_group', baseRef }).ok, false);
  }
  assert.equal(pc.branchPolicy({ eventName: 'push', baseRef: 'main' }).kind, 'not-pr');
});

const workflow = (name) => readFileSync(resolve(__dirname, `../../.github/workflows/${name}.yml`), 'utf8');

test('generated workflow runs only on integration and serializes its reserved branch', () => {
  const text = workflow('generated');
  assert.match(text, /branches: \[develop\]/);
  assert.match(text, /github.ref == 'refs\/heads\/develop'/);
  assert.match(text, /ref: \$\{\{ github.sha \}\}/);
  assert.match(text, /group: generated-files/);
  assert.match(text, /cancel-in-progress: false/);
  assert.match(text, /chore\/generated-files/);
  assert.doesNotMatch(text, /pull_request(?:_target)?:/);
});

test('generated workflow has no bypass credential or direct stable write', () => {
  const text = workflow('generated');
  assert.match(text, /pull-requests: write/);
  assert.doesNotMatch(text, /ssh-key:|DEPLOY_KEY|secrets\.(?!GITHUB_TOKEN\b)/);
  assert.doesNotMatch(text, /\[skip ci\]|--force|\+HEAD:|git push[^\n]*\bmain\b/);
  assert.doesNotMatch(text, /approve|enablePullRequestAutoMerge|gh pr (?:ready|merge)|--auto/);
});

test('PR metadata jobs never execute PR code with a write token', () => {
  const text = workflow('pr-checks');
  assert.doesNotMatch(text, /^  pull_request_target:/m);
  const label = text.slice(text.indexOf('\n  label:'));
  assert.match(label, /ref: \$\{\{ github\.event\.pull_request\.base\.sha \}\}/);
  assert.match(label, /persist-credentials: false/);
});

function generatedBlock(id, field, indent) {
  const step = workflow('generated').split(/^      - /m).find((s) => s.includes(`\n        id: ${id}\n`));
  assert.ok(step, `generated workflow has a ${id} step`);
  const block = step.match(new RegExp(`^${' '.repeat(indent - 2)}${field}: \\|\\n((?:(?:${' '.repeat(indent)}.*|)\\n?)*)`, 'm'));
  assert.ok(block, `${id} contains executable ${field}`);
  return block[1].replace(new RegExp(`^${' '.repeat(indent)}`, 'gm'), '');
}

async function runGeneratedScript(id, pulls, { createError, updateError } = {}) {
  const failures = [], outputs = {}, requests = [], notices = [];
  const github = {
    rest: { pulls: {
      list: async (args) => { requests.push(['list', args]); return { data: pulls }; },
      create: async (args) => {
        requests.push(['create', args]);
        if (createError) throw createError;
        return { data: { number: 42, html_url: 'https://example.test/pull/42' } };
      },
      update: async (args) => {
        requests.push(['update', args]);
        if (updateError) throw updateError;
        return { data: { number: 42, html_url: 'https://example.test/pull/42' } };
      },
    } },
  };
  const core = {
    setFailed: (m) => failures.push(m), setOutput: (k, v) => { outputs[k] = v; },
    info: (m) => notices.push(m), warning: (m) => notices.push(m),
  };
  const context = { repo: { owner: 'intgral-ai', repo: 'open-skyfall-desktop' } };
  const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
  await new AsyncFunction('github', 'context', 'core', generatedBlock(id, 'script', 12))(github, context, core);
  return { failures, outputs, requests, notices };
}

test('generated preflight blocks an already-ready PR', async () => {
  const result = await runGeneratedScript('draft', [{ number: 42, draft: false, base: { ref: 'develop' } }]);
  assert.equal(result.failures.length, 1);
  assert.match(result.failures[0], /blocked/i);
  assert.deepEqual(result.requests.map(([operation]) => operation), ['list']);
});

test('generated workflow creates a draft integration PR and requires a human CI trigger', async () => {
  const result = await runGeneratedScript('pull-request', []);
  assert.deepEqual(result.failures, []);
  const request = result.requests.find(([operation]) => operation === 'create')[1];
  assert.equal(request.draft, true);
  assert.equal(request.base, 'develop');
  assert.equal(request.head, 'chore/generated-files');
  assert.match(request.body, /ready.for.review/i);
  assert.match(request.body, /real checks/i);
  assert.match(request.body, /Changes a default: no/);
  assert.match(result.notices.join('\n'), /CI.*(?:not|never)|(?:not|never).*CI/i);
});

test('generated workflow updates only an existing draft PR', async () => {
  const result = await runGeneratedScript('pull-request', [{ number: 42, draft: true, base: { ref: 'develop' } }]);
  assert.deepEqual(result.failures, []);
  assert.deepEqual(result.requests.map(([operation]) => operation), ['list', 'update']);
  assert.equal(result.requests[1][1].pull_number, 42);
});

test('generated workflow fails visibly when the repository token cannot create a PR', async () => {
  const result = await runGeneratedScript('pull-request', [], { createError: Object.assign(new Error('Not permitted'), { status: 403 }) });
  assert.equal(result.failures.length, 1);
  assert.match(result.failures[0], /blocked/i);
  assert.match(result.failures[0], /permission|maintainer/i);
  assert.deepEqual(result.requests.map(([operation]) => operation), ['list', 'create']);
});

test('generated PR update failure never falls back to a different write', async () => {
  const result = await runGeneratedScript('pull-request', [{ number: 42, draft: true, base: { ref: 'develop' } }], { updateError: new Error('Service unavailable') });
  assert.equal(result.failures.length, 1);
  assert.deepEqual(result.requests.map(([operation]) => operation), ['list', 'update']);
});

test('generated workflow refuses ready or misdirected PRs at both metadata boundaries', async () => {
  for (const id of ['draft', 'pull-request']) {
    for (const pulls of [
      [{ number: 42, draft: false, base: { ref: 'develop' } }],
      [{ number: 42, draft: true, base: { ref: 'main' } }],
      [{ draft: true, base: { ref: 'develop' } }, { draft: true, base: { ref: 'develop' } }],
    ]) {
      const result = await runGeneratedScript(id, pulls);
      assert.equal(result.failures.length, 1);
      assert.deepEqual(result.requests.map(([operation]) => operation), ['list']);
    }
  }
});

// Execute the workflow's real shell against local bare remotes. No network or public writes.
function generationFixture(t) {
  const root = mkdtempSync(join(tmpdir(), 'solidcraft-generated-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const env = { ...process.env, GIT_CONFIG_NOSYSTEM: '1', GIT_CONFIG_GLOBAL: join(root, 'gitconfig'), RUNNER_TEMP: root, GITHUB_OUTPUT: join(root, 'output') };
  writeFileSync(env.GIT_CONFIG_GLOBAL, '[user]\n name = Generator test\n email = generator@example.test\n[commit]\n gpgsign = false\n');
  const git = (cwd, ...args) => execFileSync('git', args, { cwd, env, encoding: 'utf8', stdio: 'pipe' }).trim();
  const seed = join(root, 'seed'), remote = join(root, 'remote.git');
  mkdirSync(seed);
  git(seed, 'init', '--initial-branch=develop');
  for (const [file, content] of Object.entries({
    'THIRD-PARTY-LICENSES.txt': 'licenses\n', 'docs/features.md': 'features\n',
    'assets/locales/example.json': '{}\n', 'ATTRIBUTION.md': 'attribution\n',
    'attribution/pending/example.md': 'pending\n', 'README.md': 'base\n', 'Cargo.toml': '# trusted baseline\n',
  })) {
    mkdirSync(resolve(seed, file, '..'), { recursive: true });
    writeFileSync(join(seed, file), content);
  }
  git(seed, 'add', '.');
  git(seed, 'commit', '-m', 'Integration baseline');
  git(seed, 'branch', 'main');
  git(root, 'clone', '--bare', seed, remote);
  git(seed, 'remote', 'add', 'origin', remote);
  const baseline = git(seed, 'rev-parse', 'HEAD');
  let clones = 0;
  const clone = () => {
    const cwd = join(root, `runner-${clones++}`);
    git(root, 'clone', '--branch', 'develop', remote, cwd);
    return cwd;
  };
  const capture = (cwd) => {
    assert.equal(typeof pc.captureGenerated, 'function');
    return pc.captureGenerated({ cwd, sourceRevision: git(cwd, 'rev-parse', 'HEAD') });
  };
  const prepare = (payload, cwd = clone()) => {
    assert.equal(typeof pc.prepareGenerated, 'function');
    return { cwd, ...pc.prepareGenerated({ cwd, sourceRevision: git(cwd, 'rev-parse', 'HEAD'), payload }) };
  };
  const push = async (prepared, pulls = []) => {
    assert.equal(typeof pc.pushGenerated, 'function');
    const github = { rest: {
      pulls: { list: async () => ({ data: pulls }) },
      git: { getRef: async () => {
        const sha = git(remote, 'for-each-ref', '--format=%(objectname)', 'refs/heads/chore/generated-files');
        if (!sha) throw Object.assign(new Error('Not found'), { status: 404 });
        return { data: { object: { sha } } };
      } },
    } };
    await pc.pushGenerated({ ...prepared, github, context: { repo: { owner: 'fixture', repo: 'local' } }, token: 'local-fixture-token' });
  };
  const reserved = (change) => {
    const cwd = clone();
    git(cwd, 'switch', '-c', 'chore/generated-files');
    change(cwd);
    git(cwd, 'add', '-A');
    git(cwd, 'commit', '-m', 'Reserved branch change');
    git(cwd, 'push', 'origin', 'chore/generated-files');
    return git(cwd, 'rev-parse', 'HEAD');
  };
  return { root, env, seed, remote, baseline, git, clone, capture, prepare, push, reserved };
}

test('unchanged generation creates no remote branch or stable write', (t) => {
  const f = generationFixture(t), prepared = f.prepare(f.capture(f.clone()));
  assert.equal(prepared.changed, false);
  assert.equal(f.git(f.remote, 'for-each-ref', '--format=%(refname)', 'refs/heads/chore/generated-files'), '');
  assert.equal(f.git(f.remote, 'rev-parse', 'develop'), f.baseline);
  assert.equal(f.git(f.remote, 'rev-parse', 'main'), f.baseline);
});

test('generation updates preserve ancestry without mistaking trusted Cargo progress for pollution', async (t) => {
  const f = generationFixture(t), first = f.clone();
  writeFileSync(join(first, 'docs/features.md'), 'generated first\n');
  await f.push(f.prepare(f.capture(first)));
  const previous = f.git(f.remote, 'rev-parse', 'chore/generated-files');
  writeFileSync(join(f.seed, 'README.md'), 'new integration work\n');
  writeFileSync(join(f.seed, 'Cargo.toml'), '# trusted new integration configuration\n');
  f.git(f.seed, 'commit', '-am', 'New integration work');
  f.git(f.seed, 'push', 'origin', 'develop');
  const integration = f.git(f.seed, 'rev-parse', 'HEAD'), next = f.clone();
  writeFileSync(join(next, 'docs/features.md'), 'generated second\n');
  const prepared = f.prepare(f.capture(next));
  assert.equal(f.git(prepared.cwd, 'rev-parse', 'HEAD'), integration, 'writer never checks out the reserved tree');
  await f.push(prepared);
  const generated = f.git(f.remote, 'rev-parse', 'chore/generated-files');
  assert.notEqual(generated, previous);
  assert.equal(f.git(f.remote, 'merge-base', previous, generated), previous);
  assert.equal(f.git(f.remote, 'merge-base', integration, generated), integration);
  assert.equal(f.git(f.remote, 'show', `${generated}:docs/features.md`), 'generated second');
  assert.equal(f.git(f.remote, 'rev-parse', 'develop'), integration);
  assert.equal(f.git(f.remote, 'rev-parse', 'main'), f.baseline);
  assert.equal(prepared.changed, true);
  assert.equal(f.git(f.remote, 'show', `${generated}:Cargo.toml`), '# trusted new integration configuration');
  assert.doesNotMatch(f.git(f.remote, 'log', '--format=%s', generated), /\[skip ci\]/);
});

test('a denied generation push fails without a fallback branch write', async (t) => {
  const f = generationFixture(t), cwd = f.clone();
  const hook = join(f.remote, 'hooks/pre-receive');
  writeFileSync(hook, '#!/bin/sh\nexit 1\n');
  chmodSync(hook, 0o755);
  writeFileSync(join(cwd, 'docs/features.md'), 'generated\n');
  await assert.rejects(f.push(f.prepare(f.capture(cwd))), /Generation blocked/);
  assert.equal(f.git(f.remote, 'for-each-ref', '--format=%(refname)', 'refs/heads/chore/generated-files'), '');
  assert.equal(f.git(f.remote, 'rev-parse', 'develop'), f.baseline);
  assert.equal(f.git(f.remote, 'rev-parse', 'main'), f.baseline);
});

test('a generation merge conflict blocks without rewriting the reserved branch', async (t) => {
  const f = generationFixture(t), cwd = f.clone();
  writeFileSync(join(cwd, 'docs/features.md'), 'generated\n');
  await f.push(f.prepare(f.capture(cwd)));
  const previous = f.git(f.remote, 'rev-parse', 'chore/generated-files');
  writeFileSync(join(f.seed, 'docs/features.md'), 'conflicting integration\n');
  f.git(f.seed, 'commit', '-am', 'Concurrent integration edit');
  f.git(f.seed, 'push', 'origin', 'develop');
  assert.throws(() => f.prepare(f.capture(f.clone())), /Generation blocked.*conflict/i);
  assert.equal(f.git(f.remote, 'rev-parse', 'chore/generated-files'), previous);
  assert.equal(f.git(f.remote, 'rev-parse', 'main'), f.baseline);
});

test('reserved Cargo.toml pollution is rejected before entering a generator checkout', (t) => {
  const f = generationFixture(t);
  const expected = f.reserved((cwd) => writeFileSync(join(cwd, 'Cargo.toml'), '# unreviewed build configuration\n'));
  const runner = f.clone();
  assert.throws(() => f.prepare(f.capture(runner), runner), /Generation blocked.*Cargo.toml/);
  assert.equal(f.git(runner, 'rev-parse', 'HEAD'), f.baseline);
  assert.equal(readFileSync(join(runner, 'Cargo.toml'), 'utf8'), '# trusted baseline\n');
  assert.equal(f.git(f.remote, 'rev-parse', 'chore/generated-files'), expected);
});

for (const path of ['.cargo/config.toml', 'assets/locales/extra.json', 'attribution/pending/README.md']) {
  test(`reserved non-generated path ${path} blocks without being discarded`, (t) => {
    const f = generationFixture(t);
    const expected = f.reserved((cwd) => {
      mkdirSync(resolve(cwd, path, '..'), { recursive: true });
      writeFileSync(join(cwd, path), 'unreviewed\n');
    });
    assert.throws(() => f.prepare(f.capture(f.clone())), /Generation blocked/);
    assert.equal(f.git(f.remote, 'rev-parse', 'chore/generated-files'), expected);
  });
}

for (const mode of ['symlink', 'executable']) {
  test(`reserved generated ${mode} is rejected as an unsafe mode`, (t) => {
    const f = generationFixture(t);
    const expected = f.reserved((cwd) => {
      const path = join(cwd, 'docs/features.md');
      if (mode === 'symlink') { rmSync(path); symlinkSync('../Cargo.toml', path); }
      else chmodSync(path, 0o755);
    });
    assert.throws(() => f.prepare(f.capture(f.clone())), /Generation blocked.*docs\/features.md/);
    assert.equal(f.git(f.remote, 'rev-parse', 'chore/generated-files'), expected);
  });
}

test('generator output capture rejects code changes and symbolic-link output', (t) => {
  const f = generationFixture(t), cwd = f.clone();
  writeFileSync(join(cwd, 'Cargo.toml'), '# unexpected generator change\n');
  assert.throws(() => f.capture(cwd), /Generation blocked.*Cargo.toml/);
  f.git(cwd, 'restore', 'Cargo.toml');
  rmSync(join(cwd, 'docs/features.md'));
  symlinkSync('../Cargo.toml', join(cwd, 'docs/features.md'));
  assert.throws(() => f.capture(cwd), /Generation blocked.*docs\/features.md/);
});

test('writer rejects hostile artifact paths, pending additions and revision mismatch', (t) => {
  const f = generationFixture(t), payload = f.capture(f.clone());
  for (const path of ['Cargo.toml', '../Cargo.toml', 'assets/locales/extra.json', 'attribution/pending/README.md', 'attribution/pending/example.md']) {
    assert.throws(() => f.prepare({ ...payload, files: [...payload.files, { path, content: Buffer.from('hostile').toString('base64') }] }), /Generation blocked/);
  }
  assert.throws(() => f.prepare({ ...payload, sourceRevision: '0'.repeat(40) }), /Generation blocked/);
  assert.throws(() => f.prepare({ ...payload, files: [...payload.files, payload.files[0]] }), /Generation blocked/);
});

test('folded pending deletion and regular output refresh reach only the reserved branch', async (t) => {
  const f = generationFixture(t), cwd = f.clone();
  rmSync(join(cwd, 'attribution/pending/example.md'));
  writeFileSync(join(cwd, 'ATTRIBUTION.md'), 'folded attribution\n');
  const prepared = f.prepare(f.capture(cwd));
  await f.push(prepared);
  const generated = f.git(f.remote, 'rev-parse', 'chore/generated-files');
  assert.equal(f.git(f.remote, 'ls-tree', generated, 'attribution/pending/example.md'), '');
  assert.equal(f.git(f.remote, 'show', `${generated}:ATTRIBUTION.md`), 'folded attribution');
  assert.equal(f.git(f.remote, 'rev-parse', 'develop'), f.baseline);
});

test('push rechecks draft status and expected branch head immediately before its Git write', async (t) => {
  const f = generationFixture(t), cwd = f.clone();
  writeFileSync(join(cwd, 'docs/features.md'), 'generated\n');
  const prepared = f.prepare(f.capture(cwd));
  await assert.rejects(f.push(prepared, [{ draft: false, base: { ref: 'develop' } }]), /Generation blocked/);
  const concurrent = f.reserved((path) => writeFileSync(join(path, 'docs/features.md'), 'concurrent edit\n'));
  await assert.rejects(f.push(prepared), /Generation blocked.*head/i);
  assert.equal(f.git(f.remote, 'rev-parse', 'chore/generated-files'), concurrent);
});

test('Cargo generation and credentialed output maintenance use separate pinned jobs', () => {
  const text = workflow('generated'), jobs = text.split(/^  maintain:/m);
  assert.equal(jobs.length, 2, 'separate output-only writer job');
  const [generate, maintain] = jobs;
  assert.match(generate, /permissions:\n  contents: read/);
  assert.doesNotMatch(generate, /contents: write|pull-requests: write|GH_TOKEN:|GITHUB_TOKEN:/);
  assert.match(maintain, /needs: generate/);
  assert.match(maintain, /contents: write/);
  assert.doesNotMatch(maintain, /cargo |rust-toolchain|rust-cache|git (?:switch|checkout|merge) /);
  assert.equal((text.match(/persist-credentials: false/g) || []).length, 2);
  assert.equal((text.match(/ref: \$\{\{ github.sha \}\}/g) || []).length, 2);
  assert.match(maintain, /path: \$\{\{ runner.temp \}\}\/generated-output/);
});
