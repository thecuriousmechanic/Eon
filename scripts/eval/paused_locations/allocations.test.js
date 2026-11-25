'use strict';

const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { workDirectory } = require('./host');
const { host, expected } = require('./allocation_project');

function setup(t) {
  const root = workDirectory();
  const program = path.join(root, 'program');
  fs.writeFileSync(program, '', { mode: 0o700 });
  const h = host(root, program);
  t.after(async () => { await h.close(); fs.rmSync(root, { recursive: true, force: true }); });
  h.names = () => fs.readdirSync(root).filter(s => s.startsWith('.eon-alloc-'));
  h.prepare = async () => {
    h.session.configuration = await h.resolve(h.config);
    assert.match(h.session.configuration.env[0], /^EON_PROJECT_ALLOC=[0-9a-f]{32}$/);
    const [file] = h.names();
    h.file = path.join(root, file);
  };
  return Object.assign(h, { root, program });
}

// Unit-test only: author a protocol record. The native and stock desktop
// gates use project_counts.c, not this helper, to exercise real allocator calls.
function publish(h, counts = [1n, 2n, 3n, 4n], edit = () => {}) {
  const data = fs.readFileSync(h.file);
  assert.equal(data.length, 88);
  assert.equal(data.toString('ascii', 0, 8), 'EONPEND1');
  assert.equal(fs.statSync(h.file).mode & 0o777, 0o600);
  data.write('EONLIVE1', 'ascii');
  counts.forEach((n, i) => data.writeBigUInt64LE(n, 56 + 8 * i));
  edit(data);
  fs.writeFileSync(h.file, data);
  fs.unlinkSync(h.file);
}

test('plain and cancelled launches collect nothing; command takes no arguments and sends no debugger request', async t => {
  const h = setup(t);
  for (const flag of [undefined, false, 1, 'true']) {
    const c = { ...h.config, eonAllocationCounts: flag };
    assert.equal(await h.resolve(c), c);
  }
  await h.command({ session: h.session });
  assert.deepEqual(h.messages, []);
  assert.deepEqual(h.prompts, []);
  h.choose = () => undefined;
  assert.equal(await h.resolve(h.config), undefined);
  assert.deepEqual(h.prompts, ['Enable once']);
  assert.deepEqual(h.names(), []);
  await h.command();
  assert.match(h.messages[0], /^No counts shown\./);
});

test('only a simple local LLVM launch with an owned executable is eligible', async t => {
  for (const change of [
    h => { h.config.request = 'attach'; }, h => { h.config.type = 'other'; },
    h => { h.config.debugAdapterPort = 123; }, h => { h.config.launchCommands = []; },
    h => { h.config.env = ['EXISTING=keep']; }, h => { h.config.cwd += '/other'; },
    h => { h.config.program = h.root + '/./program'; }, h => { h.config.program = '/usr/bin/true'; },
    h => { h.v.workspace.isTrusted = false; }, h => { h.v.env.remoteName = 'remote'; },
    h => { h.v.env.uiKind = 2; }, h => { h.folder.uri.scheme = 'vscode-remote'; },
    h => { fs.chmodSync(h.root, 0o775); }, h => { fs.chmodSync(h.program, 0o600); },
    h => { fs.renameSync(h.program, h.program + '.real'); fs.symlinkSync(h.program + '.real', h.program); },
  ]) {
    const h = setup(t);
    change(h);
    assert.equal(await h.resolve(h.config), undefined);
    assert.deepEqual(h.prompts, []);
    assert.deepEqual(h.names(), []);
  }
});

test('launch consent is scoped to a stable folder and executable; startup failures are cleaned up', async t => {
  for (const change of [
    h => { h.v.workspace.isTrusted = false; }, h => { h.config.program += '2'; },
    h => { fs.renameSync(h.program, h.program + '.old'); fs.writeFileSync(h.program, '', { mode: 0o700 }); },
    h => { h.token.isCancellationRequested = true; },
  ]) {
    const h = setup(t);
    h.token = { isCancellationRequested: false };
    h.choose = x => { change(h); return x; };
    assert.equal(await h.resolve(h.config, h.token), undefined);
    assert.deepEqual(h.names(), []);
  }
  const h = setup(t);
  await h.prepare();
  assert.equal(await h.resolve(h.config), undefined); // one outstanding launch per window
  assert.equal(h.names().length, 1);
  await h.close();
  assert.deepEqual(h.names(), []);
});

test('only the bound root session and focused paused frame can disclose the fixed numeric schema', async t => {
  const h = setup(t);
  await h.prepare();
  publish(h);
  for (const session of [{ ...h.session, parentSession: h.session }, { ...h.session, type: 'other' },
    { ...h.session, configuration: { ...h.session.configuration, env: [] } }]) h.start(session);
  await h.command();
  assert.match(h.messages.at(-1), /^No counts shown\./);
  h.start(h.session);
  h.choose = () => undefined;
  await h.command();
  assert.equal(h.messages.length, 1);
  h.choose = action => action;
  await h.command();
  assert.equal(h.messages.at(-1), expected(1, 2, 3, 4));
  h.v.debug.activeStackItem = undefined;
  await h.command();
  assert.match(h.messages.at(-1), /^No counts shown\./);
  h.v.debug.activeStackItem = h.frame;
  await h.end(h.session);
  await h.command();
  assert.match(h.messages.at(-1), /^No counts shown\./);
});

test('unclaimed, missing, incomplete, malformed and overflow records are unavailable, never synthetic zeros', async t => {
  for (const change of [
    () => {}, h => fs.unlinkSync(h.file), h => fs.truncateSync(h.file, 55),
    h => publish(h, undefined, b => b.write('notready', 'ascii')),
    h => publish(h, undefined, b => b[9] ^= 1),
    h => publish(h, [1n << 53n, 0n, 0n, 0n]),
  ]) {
    const h = setup(t);
    await h.prepare();
    h.start(h.session);
    change(h);
    await h.command();
    assert.match(h.messages.at(-1), /^No counts shown\./);
  }
  const h = setup(t);
  await h.prepare();
  publish(h, [0n, 0n, 0n, 0n]);
  h.start(h.session);
  await h.command();
  assert.equal(h.messages[0], expected(0, 0, 0, 0));
});

test('uses the retained file handle; cleanup never removes a replacement', async t => {
  const h = setup(t);
  await h.prepare();
  h.start(h.session);
  fs.renameSync(h.file, h.file + '.old');
  fs.writeFileSync(h.file, 'replacement');
  await h.command();
  assert.match(h.messages.at(-1), /^No counts shown\./);
  await h.end(h.session);
  assert.equal(fs.readFileSync(h.file, 'utf8'), 'replacement');
});

test('consent is not transferable after focus, trust or session changes; overlapping commands are ignored', async t => {
  for (const change of [
    h => { h.v.debug.activeStackItem = undefined; }, h => { h.v.debug.activeDebugSession = { ...h.session }; },
    h => { h.v.workspace.isTrusted = false; }, h => h.end(h.session), h => { fs.chmodSync(h.root, 0o775); },
  ]) {
    const h = setup(t);
    await h.prepare();
    publish(h);
    h.start(h.session);
    h.choose = async action => { await h.command(); await change(h); return action; };
    await h.command();
    assert.deepEqual(h.prompts, ['Enable once', 'Show counts']);
    assert.match(h.messages.at(-1), /^No counts shown\./);
  }
});
