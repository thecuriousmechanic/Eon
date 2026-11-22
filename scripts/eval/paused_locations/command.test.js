'use strict';

const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { editor, workDirectory } = require('./host');

function setup(t) {
  const work = workDirectory();
  t.after(() => fs.rmSync(work, { recursive: true, force: true }));
  const root = path.join(work, 'project');
  fs.mkdirSync(root);
  const text = 'one\ntwo\nthree\n';
  const file = path.join(root, 'two words.rs');
  fs.writeFileSync(file, text, { mode: 0o600 });
  const h = editor(root, [[file, text]], (cmd, args) => {
    h.calls.push([cmd, args]);
    return h.reply();
  });
  h.calls = [];
  h.reply = () => ({ stackFrames: [{ source: { path: file }, line: 3 }] });
  h.pick = items => items;
  return Object.assign(h, { work, root, file, text });
}

test('only the declared location/count commands and narrow launch provider activate; location consent is per invocation', async t => {
  const h = setup(t);
  const m = require('../../../integrations/vscode-paused-locations/package.json');
  assert.deepEqual(m.activationEvents, [`onCommand:${h.name}`, 'onCommand:eon.showProjectAllocationCounts', 'onDebugResolve:lldb-dap']);
  assert.deepEqual(m.contributes.commands.map(c => c.command), [h.name, 'eon.showProjectAllocationCounts']);
  assert.deepEqual(h.calls, []);
  await h.command({ threadId: 10, files: [h.file] });
  assert.deepEqual(h.offered, []);
  for (const answer of [undefined, [], ['other.rs'], Array(9).fill('two words.rs')]) {
    h.pick = () => answer;
    await h.command();
  }
  assert.equal(h.offered.length, 4);
  assert.equal(h.calls.length, 0);
  h.pick = items => items;
  await h.command();
  assert.deepEqual(h.calls, [['stackTrace', { threadId: 1, startFrame: 0, levels: 12 }]]);
  assert.equal(h.messages.at(-1), 'two words.rs:3');
  h.pick = () => undefined;
  await h.command();
  assert.equal(h.calls.length, 1);
});

test('requires a local trusted folder and focused frame in the same active session', async t => {
  const setters = [
    h => { h.v.workspace.isTrusted = false; },
    h => { h.v.env.remoteName = 'ssh-remote'; },
    h => { h.v.env.uiKind = 2; },
    h => { h.v.debug.activeDebugSession = undefined; },
    h => { h.v.debug.activeStackItem = { session: h.session, threadId: 1 }; },
    h => { h.v.debug.activeStackItem = undefined; },
    h => { h.frame.session = {}; },
    h => { h.frame.threadId = -1; },
    h => { h.frame.frameId = NaN; },
    h => { h.folder.uri.scheme = 'vscode-remote'; },
    h => { h.folder.uri.fsPath = '/'; },
    h => { h.v.workspace.workspaceFolders = []; },
  ];
  for (const change of setters) {
    const h = setup(t);
    change(h);
    await h.command();
    assert.deepEqual(h.offered, []);
    assert.deepEqual(h.calls, []);
  }
});

test('only clean ordinary open text tabs in the session folder are offered', async t => {
  for (const change of [
    h => { h.docs[0].isDirty = true; }, h => { h.docs[0].isUntitled = true; },
    h => { h.docs[0].isClosed = true; }, h => { h.tabs[0].isDirty = true; },
    h => { h.tabs.length = 0; }, h => { h.tabs[0].input = { uri: h.docs[0].uri }; },
    h => { h.docs[0].uri.fsPath += '.json'; }, h => { h.docs[0].uri.fsPath = `${h.root}/.git/foo.rs`; },
    h => { h.docs[0].uri.fsPath = `${h.root}/x/../two words.rs`; },
    h => { h.docs[0].uri.fsPath = `${h.root}2/foreign.rs`; },
    h => { h.docs[0].uri.fsPath = `${h.root}/a\u202e.rs`; },
    h => { h.docs[0].uri.scheme = 'untitled'; }, h => { h.docs[0].uri.authority = 'host'; },
    h => { h.docs[0].uri.query = 'q'; }, h => { h.docs[0].uri.fragment = 'f'; },
    h => { h.docs[0].lineCount = 100_001; }, h => { h.v.workspace.getWorkspaceFolder = () => undefined; },
  ]) {
    const h = setup(t);
    change(h);
    await h.command();
    assert.equal(h.offered.length, 0);
    assert.equal(h.calls.length, 0);
  }
});

test('binds reads to volunteered regular files, checks permissions, links, size and saved bytes', async t => {
  for (const change of [
    h => fs.chmodSync(h.file, 0o662),
    h => fs.chmodSync(h.root, 0o775),
    h => fs.linkSync(h.file, path.join(h.work, 'alias.rs')),
    h => { fs.renameSync(h.file, `${h.file}.real`); fs.symlinkSync(`${h.file}.real`, h.file); },
    h => { fs.renameSync(h.root, `${h.root}.real`); fs.symlinkSync(`${h.root}.real`, h.root); },
    h => { fs.unlinkSync(h.file); fs.mkdirSync(h.file); },
    h => fs.truncateSync(h.file, 2 * 1024 * 1024 + 1),
    h => fs.writeFileSync(h.file, 'unseen external save'),
    h => { fs.writeFileSync(h.file, '\0'); h.docs[0].getText = () => '\0'; },
  ]) {
    const h = setup(t);
    change(h);
    await h.command();
    assert.equal(h.calls.length, 0);
    assert.match(h.messages.at(-1), /^No locations shown\./);
  }
  const h = setup(t);
  fs.writeFileSync(h.file, '\ufeff' + h.text);
  await h.command();
  assert.equal(h.messages[0], 'two words.rs:3');
});

test('no adapter-provided name, value, alias, source reference or unselected path leaves the command', async t => {
  const h = setup(t);
  const other = path.join(h.root, 'unselected.py');
  let reads = 0;
  h.docs.push({ ...h.docs[0], uri: { ...h.docs[0].uri, fsPath: other }, getText() { reads++; } });
  h.tabs.push({ input: Object.assign(new h.v.TabInputText(), { uri: h.docs[1].uri }) });
  h.pick = () => ['two words.rs'];
  const row = (p, line, ref) => ({ name: '\x1b[1msecret', source: { path: p, sourceReference: ref, name: 'secret' },
    line, instructionPointerReference: 'secret', moduleId: 'secret' });
  h.reply = () => ({ stackFrames: [
    row(other, 2), row(h.file, 2, 2), row(h.file, 2, null), row(h.file, 1.5), row(h.file, 0),
    row(h.file, 5), row('file://' + h.file, 2), row(h.root + '/./two words.rs', 2),
    row('two words.rs', 2), row('x'.repeat(4097), 2), row(h.file, 1, 0), row(h.file, 3),
  ] });
  await h.command();
  assert.equal(reads, 0);
  assert.deepEqual(h.messages, ['two words.rs:1\ntwo words.rs:3']);
});

test('rejects malformed or oversized replies and never relays adapter errors', async t => {
  const h = setup(t);
  for (const body of [null, {}, { stackFrames: {} }, { stackFrames: Array(13).fill({ source: { path: h.file }, line: 1 }) }]) {
    h.reply = () => body;
    await h.command();
    assert.match(h.messages.at(-1), /^No locations shown\./);
  }
  h.reply = () => { throw Error('SENSITIVE ADAPTER DIAGNOSTIC'); };
  await h.command();
  assert.match(h.messages.at(-1), /^No locations shown\./);
  assert.ok(h.messages.every(msg => !msg.includes('SENSITIVE') && !msg.includes(h.root)));
});

test('labels only enabled conditional source breakpoints on selected frame lines, without evaluation', async t => {
  const h = setup(t);
  h.v.SourceBreakpoint = class {};
  const point = (line, opts = {}) => Object.assign(new h.v.SourceBreakpoint(), {
    enabled: true, location: { uri: h.docs[0].uri, range: { start: { line } } }, ...opts,
  });
  const unrelated = point(0);
  Object.defineProperty(unrelated, 'condition', { get() { throw Error('unselected condition was read'); } });
  for (const opts of [{ condition: 'never expose this' }, { hitCondition: '7' }]) {
    h.v.debug.breakpoints = [unrelated, point(2, opts)];
    await h.command();
    assert.equal(h.messages.at(-1), 'two words.rs:3 [conditional breakpoint configured]');
  }
  for (const points of [[point(2, { condition: 'private', enabled: false })],
    [point(2, { logMessage: 'private', condition: 'private' })], [point(2)], Array(257).fill(point(2))]) {
    h.v.debug.breakpoints = points;
    await h.command();
    assert.equal(h.messages.at(-1), 'two words.rs:3');
  }
  assert.equal(h.calls.length, 6);
  assert.ok(h.calls.every(([cmd]) => cmd === 'stackTrace'));
});

test('re-checks focus, exact document/version and files across consent and the outstanding request', async t => {
  for (const when of ['picker', 'reply']) {
    for (const change of [
      h => { h.v.debug.activeStackItem = undefined; },
      h => { h.v.debug.activeDebugSession = { ...h.session }; },
      h => { h.v.workspace.isTrusted = false; }, h => { h.tabs.length = 0; },
      h => { h.docs[0].version++; }, h => { h.docs[0].isDirty = true; },
      h => { h.docs[0] = { ...h.docs[0] }; },
      h => fs.writeFileSync(h.file, 'external save'),
      h => fs.chmodSync(h.root, 0o777),
    ]) {
      const h = setup(t);
      if (when === 'picker') h.pick = items => { change(h); return items; };
      else h.reply = () => { change(h); return { stackFrames: [{ source: { path: h.file }, line: 3 }] }; };
      await h.command();
      assert.equal(h.calls.length, when === 'picker' ? 0 : 1);
      assert.match(h.messages.at(-1), /^No locations shown\./);
    }
  }
});

test('a timed out adapter gets no retries or overlapping requests; stale result is discarded', async t => {
  const h = setup(t);
  let release, sent;
  const started = new Promise(resolve => { sent = resolve; });
  h.reply = () => new Promise(resolve => { release = resolve; sent(); });
  const timedOut = h.command();
  await started;
  await h.command();
  assert.equal(h.calls.length, 1);
  await timedOut;
  await h.command();
  assert.equal(h.calls.length, 1);
  release({ stackFrames: [{ source: { path: h.file }, line: 3 }] });
  await new Promise(resolve => setImmediate(resolve));
  assert.ok(!h.messages.some(msg => msg.includes('two words.rs:3')));
  h.reply = () => ({ stackFrames: [] });
  await h.command();
  assert.equal(h.calls.length, 2);
  assert.match(h.messages.at(-1), /^No selected file matched/);
});
