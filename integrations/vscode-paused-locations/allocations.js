'use strict';

const { open, lstat, unlink } = require('node:fs/promises');
const { O_RDWR, O_CREAT, O_EXCL, O_NOFOLLOW } = require('node:fs').constants;
const { randomBytes } = require('node:crypto');
const path = require('node:path').posix;
const { focus } = require('./command');
const { localPath, directoryId, executableStat } = require('./sources');

const command = 'eon.showProjectAllocationCounts';
const prefix = 'EON_PROJECT_ALLOC=';
const fields = ['Successful malloc calls', 'Successful calloc calls', 'Successful positive-size realloc calls', 'Non-null free calls'];
const bytes = 88; // magic[8], nonce[32], dev+ino, four LE uint64 counters (project_counts.c).
const unavailable = 'No counts shown. Use an opted-in, linked local launch and select a paused call frame.';
const launchKeys = new Set(['type', 'name', 'request', 'program', 'cwd', 'args', 'disableASLR', 'env',
  'eonAllocationCounts', 'noDebug', '__configurationTarget', '__sessionId']);

function localRoot(v, folder) {
  const root = localPath(folder?.uri);
  if (process.platform === 'linux' && process.arch === 'x64' && v.env.uiKind === v.UIKind.Desktop &&
      !v.env.remoteName && v.workspace.isTrusted && root && root !== '/' &&
      v.workspace.workspaceFolders?.some(f => localPath(f.uri) === root)) return root;
}

function profile(root, c) {
  return root && c?.type === 'lldb-dap' && c.request === 'launch' && c.eonAllocationCounts === true &&
    !c.noDebug && Object.keys(c).every(k => launchKeys.has(k)) && c.cwd === root &&
    typeof c.program === 'string' && c.program.length <= 4096 && c.program.startsWith(root + '/') &&
    path.normalize(c.program) === c.program && !/[\x00-\x1f\x7f\\]/.test(c.program);
}

function canPrepare(root, c) {
  return profile(root, c) && (c.env === undefined || (Array.isArray(c.env) && c.env.length === 0));
}

async function discard(r) {
  if (!r.handle) return;
  try {
    if (await directoryId(r.root) !== r.dir) return;
    const held = await r.handle.stat({ bigint: true });
    if (!held.nlink) return; // Claimed: the producer already unlinked it.
    const named = await lstat(r.file, { bigint: true });
    if (named.dev === held.dev && named.ino === held.ino) await unlink(r.file);
  } catch { /* Never remove a substitute, or expose filesystem errors. */ }
  finally { await r.handle.close(); }
}

async function snapshot(r) {
  const st = await r.handle.stat({ bigint: true });
  if (!st.isFile() || st.size !== BigInt(bytes) || st.nlink !== 0n ||
      await directoryId(r.root) !== r.dir) throw Error();
  async function read() {
    const b = Buffer.alloc(bytes);
    if ((await r.handle.read(b, 0, bytes, 0)).bytesRead !== bytes ||
        !b.subarray(0, 56).equals(r.header)) throw Error();
    return b;
  }
  const first = await read();
  if (!first.equals(await read())) throw Error(); // No retry on a changing snapshot.
  return fields.map((label, i) => {
    const n = first.readBigUInt64LE(56 + i * 8);
    if (n > BigInt(Number.MAX_SAFE_INTEGER)) throw Error();
    return `${label}: ${n}`;
  }).join('\n');
}

function register(v) {
  let slot, preparing = false, reading = false, closed = false;
  const modal = (detail, ...actions) => v.window.showInformationMessage('Project allocation counts', { modal: true, detail }, ...actions);
  async function release(r) {
    if (!r || slot !== r) return;
    slot = undefined;
    clearTimeout(r.timer);
    await discard(r);
  }
  const provider = {
    async resolveDebugConfigurationWithSubstitutedVariables(folder, c, cancel) {
      if (c?.eonAllocationCounts !== true) return c;
      const root = localRoot(v, folder);
      // Refuse the opted-in launch rather than silently broadening its scope.
      if (closed || cancel?.isCancellationRequested) return;
      if (preparing || slot || !canPrepare(root, c)) {
        await modal('Counting needs one simple local LLVM launch; see the extension README.');
        return;
      }
      preparing = true;
      let r;
      try {
        const program = c.program;
        const dir = await directoryId(root);
        const exe = await executableStat(root, program);
        async function check() {
          if (closed || cancel?.isCancellationRequested || localRoot(v, folder) !== root ||
              !canPrepare(root, c) || c.program !== program || await directoryId(root) !== dir) throw Error();
          const now = await executableStat(root, program);
          if (now.dev !== exe.dev || now.ino !== exe.ino) throw Error();
        }
        if (await modal('Enable four aggregate allocator-call counters for this local launch only?', 'Enable once') !== 'Enable once') return;
        await check();
        const token = randomBytes(16).toString('hex');
        const file = path.join(root, `.eon-alloc-${token}`);
        r = { dir, root, file, program, env: prefix + token };
        const record = Buffer.alloc(bytes);
        record.write('EONPEND1' + token, 'ascii');
        record.writeBigUInt64LE(exe.dev, 40);
        record.writeBigUInt64LE(exe.ino, 48);
        r.header = Buffer.from(record.subarray(0, 56));
        r.header.write('EONLIVE1', 'ascii'); // Only the matching producer marks it ready.
        r.handle = await open(file, O_RDWR | O_CREAT | O_EXCL | O_NOFOLLOW, 0o600);
        await r.handle.writeFile(record);
        await check();
        if (closed || cancel?.isCancellationRequested) throw Error();
        slot = r;
        r.timer = setTimeout(() => { void release(r); }, 60_000);
        return { ...c, env: [r.env] }; // LLVM 18 DAP uses an array of KEY=VALUE strings.
      } catch {
        if (r) await discard(r);
        await modal('Allocation counting was not enabled.');
      } finally { preparing = false; }
    },
  };
  function matches(r, session) {
    const c = session?.configuration;
    return session && !session.parentSession && session.type === 'lldb-dap' &&
      localRoot(v, session.workspaceFolder) === r.root && profile(r.root, c) && c.program === r.program &&
      Array.isArray(c.env) && c.env.length === 1 && c.env[0] === r.env;
  }
  const subscriptions = [
    v.debug.registerDebugConfigurationProvider('lldb-dap', provider),
    v.debug.onDidStartDebugSession(session => {
      const r = slot;
      if (r && !r.session && matches(r, session)) {
        r.session = session;
        clearTimeout(r.timer);
      }
    }),
    v.debug.onDidTerminateDebugSession(session => {
      if (slot?.session === session) return release(slot);
    }),
    v.commands.registerCommand(command, async (...args) => {
      if (args.length || reading) return;
      reading = true;
      try {
        const start = focus(v);
        const r = slot;
        function check() {
          if (!start || !r || slot !== r || r.session !== start.session || !matches(r, start.session) ||
              focus(v)?.frame !== start.frame) throw Error();
        }
        check();
        if (await modal('Show the four aggregate counters for this paused launch?', 'Show counts') !== 'Show counts') return;
        check();
        const detail = await snapshot(r);
        check();
        await modal(detail);
      } catch { await modal(unavailable); }
      finally { reading = false; }
    }),
  ];
  return { subscriptions, close() { closed = true; return release(slot); } };
}

module.exports = { register };
