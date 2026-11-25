'use strict';

// Explicit Linux x86-64 evidence for one reviewed, side-effect-free call in two
// production eon-voice files. No project/program/adapter/breakpoint arguments.
const fs = require('node:fs');
const path = require('node:path');
const { spawn, spawnSync } = require('node:child_process');
const { once } = require('node:events');
const assert = require('node:assert/strict');
const { repo, workDirectory, editor } = require('./host');
const { prepare, checked, inputs, llvm, project } = require('./subject');

// Bounded, private stdio peer for this one scripted interaction. No reverse requests.
async function* messages(stdout) {
  let buf = Buffer.alloc(0);
  let count = 0;
  for await (const chunk of stdout) {
    buf = Buffer.concat([buf, chunk]);
    while (buf.length) {
      const end = buf.indexOf('\r\n\r\n');
      if (end < 0) { assert.ok(buf.length < 128); break; }
      assert.ok(end < 128);
      const m = /^Content-Length: ([0-9]{1,5})$/i.exec(buf.toString('ascii', 0, end));
      assert.ok(m);
      const length = Number(m[1]);
      assert.ok(length > 0 && length <= 65536);
      if (buf.length < end + 4 + length) break;
      const msg = JSON.parse(buf.toString('utf8', end + 4, end + 4 + length));
      buf = buf.subarray(end + 4 + length);
      assert.ok(++count <= 96 && (msg.type === 'response' || msg.type === 'event'));
      if (msg.type === 'response' || ['initialized', 'stopped', 'exited', 'terminated'].includes(msg.event)) yield msg;
    }
  }
}

async function verify(work, allocations) {
  const counts = require('./allocation_project');
  const subject = allocations ? counts.build(work) : prepare(work);
  const { env } = subject;
  const { files, lines } = allocations ? { files: [[subject.file, subject.source]], lines: [subject.line] } : subject;
  const hCounts = allocations ? counts.host(work, subject.program) : undefined;
  const guard = path.join(repo, 'engine/core-rs/crates/eon-cyber/fixtures/owned_dap/launch.c');
  fs.writeFileSync(path.join(work, 'guard.c'), checked(guard, '5381158e94b80aa431059c32dd963432c0f545c36b0519cc06e825fcd6ad2128'));
  assert.equal(spawnSync(`${llvm}/clang`, ['--no-default-config', '--ld-path=/usr/bin/ld', '-O2', 'guard.c', '-o', 'guard'],
    { cwd: work, env, stdio: 'ignore', timeout: 30_000 }).status, 0);

  const guardProcess = spawn(path.join(work, 'guard'), [], { cwd: work, env, stdio: ['pipe', 'pipe', 'ignore'] });
  const closed = once(guardProcess, 'close');
  guardProcess.stdin.on('error', () => {}); // An early close is detected on stdout.
  const stream = messages(guardProcess.stdout);
  const inbox = [];
  let seq = 0;
  function send(command, args) {
    const body = JSON.stringify({ seq: ++seq, type: 'request', command, arguments: args });
    guardProcess.stdin.write(`Content-Length: ${Buffer.byteLength(body)}\r\n\r\n${body}`);
    return seq;
  }
  async function take(predicate) {
    for (;;) {
      const i = inbox.findIndex(predicate);
      if (i >= 0) return inbox.splice(i, 1)[0];
      const next = await stream.next();
      assert.ok(!next.done);
      inbox.push(next.value);
    }
  }
  const event = name => take(m => m.type === 'event' && m.event === name);
  async function reply(id) {
    const m = await take(m => m.type === 'response' && m.request_seq === id);
    assert.ok(m.success);
    return m.body;
  }
  const call = (command, args) => reply(send(command, args));
  try {
    await call('initialize', { adapterID: 'lldb', pathFormat: 'path', linesStartAt1: true, columnsStartAt1: true,
      supportsRunInTerminalRequest: false, supportsStartDebuggingRequest: false });
    const launchConfig = allocations ? await hCounts.resolve(hCounts.config) :
      { program: path.join(work, 'preview'), cwd: work, disableASLR: false };
    if (allocations) {
      assert.equal(launchConfig.env.length, 1);
      hCounts.session.configuration = launchConfig;
      hCounts.start(hCounts.session);
    }
    const launch = send('launch', launchConfig);
    await event('initialized');
    const bp = (await call('setBreakpoints', { source: { path: files[0][0] }, breakpoints: [{ line: lines[0] }] })).breakpoints[0];
    assert.ok(bp.verified && bp.line === lines[0]);
    await call('configurationDone', {});
    await reply(launch);
    async function stop() {
      const { reason, threadId, hitBreakpointIds } = (await event('stopped')).body;
      assert.ok(reason === 'breakpoint' && hitBreakpointIds.includes(bp.id));
      // The editor normally fetches this frame to populate its Call Stack view.
      const top = (await call('stackTrace', { threadId, startFrame: 0, levels: 1 })).stackFrames[0];
      assert.ok(Number.isSafeInteger(top.id) && top.source.path === files[0][0] && top.line === lines[0]);
      return { threadId, top };
    }
    let { threadId, top } = await stop();

    if (allocations) {
      for (const [i, oracle] of [counts.expected(1, 1, 0, 0), counts.expected(1, 1, 1, 2)].entries()) {
        if (i) ({ threadId, top } = await stop());
        hCounts.v.debug.activeStackItem = Object.assign(new hCounts.v.DebugStackFrame(),
          { session: hCounts.session, threadId, frameId: top.id });
        await hCounts.command();
        assert.equal(hCounts.messages[i], oracle);
        await call('continue', { threadId });
      }
      assert.deepEqual(hCounts.prompts, ['Enable once', 'Show counts', 'Show counts']);
      assert.equal((await event('exited')).body.exitCode, 0);
      await event('terminated');
      await call('disconnect', { terminateDebuggee: false });
      await hCounts.end(hCounts.session);
      return hCounts.messages;
    }

    // The product command receives only the already-paused editor API, not the setup/control peer.
    let requests = 0;
    const h = editor(project, files, async (command, args) => {
      assert.equal(command, 'stackTrace');
      assert.deepEqual(args, { threadId, startFrame: 0, levels: 12 });
      requests++;
      return call('stackTrace', args);
    }, threadId, top.id);
    const expected = inputs.map(([rel], i) => `${rel}:${lines[i]}`);
    h.pick = () => inputs.map(([rel]) => rel);
    await h.command();
    assert.deepEqual(h.messages, [expected.join('\n')]);
    h.pick = () => [inputs[1][0]];
    await h.command();
    assert.equal(h.messages[1], expected[1]);
    assert.equal(requests, 2);

    await call('continue', { threadId });
    assert.equal((await event('exited')).body.exitCode, 0);
    await event('terminated');
    await call('disconnect', { terminateDebuggee: false });
    return expected;
  } finally {
    guardProcess.kill('SIGTERM'); // Existing subreaper also cleans up a still-paused test on failure.
    await hCounts?.close();
    assert.deepEqual(await closed, [0, null]);
  }
}

async function main() {
  assert.ok(process.platform === 'linux' && process.arch === 'x64');
  const allocations = process.argv[2] === '--verify-project-allocations';
  assert.deepEqual(process.argv.slice(2), [allocations ? '--verify-project-allocations' : '--verify-owned-sources']);
  const work = workDirectory();
  try {
    const expected = await verify(work, allocations);
    if (allocations) console.log(`LLVM 18 stdio; two verified project stops:\n${expected.join('\n---\n')}\nexit 0.`);
    else console.log(`LLVM 18 stdio; both: ${expected.join(', ')}; caller only: ${expected[1]}; exit 0.`);
  } finally {
    fs.rmSync(work, { recursive: true, force: true });
  }
}
main().catch(err => {
  // Report the failed check's location, never the adapter's text or raw reply.
  const line = err.stack?.match(/native\.js:(\d+):\d+/)?.[1];
  console.error(`Native paused-project check failed${line ? ` at native.js:${line}` : ''}.`);
  process.exitCode = 1;
});
