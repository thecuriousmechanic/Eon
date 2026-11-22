'use strict';

const { lstat, open } = require('node:fs/promises');
const { O_RDONLY, O_NOFOLLOW, O_NONBLOCK } = require('node:fs').constants;
const path = require('node:path').posix;

const suffix = /\.(?:c|cc|cpp|cxx|h|hh|hpp|cs|go|java|js|jsx|kt|kts|mjs|py|rb|rs|swift|ts|tsx)$/i;
const maxBytes = 2 * 1024 * 1024;
const maxLines = 100_000;

function localPath(uri) {
  if (!uri || uri.scheme !== 'file' || uri.authority || uri.query || uri.fragment) return;
  const p = uri.fsPath;
  if (typeof p === 'string' && p.length <= 4096 && p.startsWith('/') &&
      !p.startsWith('//') && path.normalize(p) === p && !/[\x00-\x1f\x7f\\]/.test(p)) return p;
}

function relativeSource(root, file) {
  if (!file || !file.startsWith(root + '/')) return;
  const rel = file.slice(root.length + 1);
  // Use only these literal editor names in UI (no markup, controls, bidi, hidden files or aliases).
  if (rel.length <= 200 && suffix.test(rel) &&
      rel.split('/').every(part => /^[A-Za-z0-9_][A-Za-z0-9_. +\-]*$/.test(part))) return rel;
}

// Only materialized, clean text tabs count; textDocuments alone includes background documents.
function candidates(v, root) {
  const open = new Set();
  const docs = v.workspace.textDocuments;
  let tabs = 0;
  if (docs.length > 256) throw Error();
  for (const group of v.window.tabGroups.all) {
    tabs += group.tabs.length;
    if (tabs > 256) throw Error();
    for (const tab of group.tabs) {
      if (!tab.isDirty && tab.input instanceof v.TabInputText) {
        const file = localPath(tab.input.uri);
        if (relativeSource(root, file)) open.add(file);
      }
    }
  }
  const options = new Map();
  for (const doc of docs) {
    const file = localPath(doc.uri);
    const label = relativeSource(root, file);
    if (!label || !open.has(file) || doc.isClosed || doc.isUntitled || doc.isDirty ||
        !Number.isSafeInteger(doc.lineCount) || doc.lineCount < 1 || doc.lineCount > maxLines ||
        localPath(v.workspace.getWorkspaceFolder(doc.uri)?.uri) !== root) continue;
    options.set(label, { doc, version: doc.version, file, label });
    if (options.size > 128) throw Error();
  }
  return options;
}

// Walk only editor-selected paths, never paths supplied by the adapter.
async function checkedPath(root, file, kind) {
  const uid = BigInt(process.geteuid());
  const components = file.slice(1).split('/');
  if (components.length > 64) throw Error();
  let prefix = '/';
  for (let i = 0; i <= components.length; i++) {
    const s = await lstat(prefix, { bigint: true });
    if ((s.mode & 0o022n) !== 0n || (s.uid !== uid && s.uid !== 0n)) throw Error();
    if (i === components.length) {
      if (kind === 'directory') {
        if (!s.isDirectory() || s.uid !== uid) throw Error();
        return `${s.dev}:${s.ino}`;
      }
      if (!s.isFile() || s.uid !== uid || s.nlink !== 1n) throw Error();
      if (kind === 'executable') {
        if (!(s.mode & 0o100n) || (s.mode & 0o6000n)) throw Error();
        return s;
      }
      if (s.size > BigInt(maxBytes)) throw Error();
      return stamp(s);
    }

    if (!s.isDirectory() || (prefix === root && s.uid !== uid)) throw Error();
    prefix = path.join(prefix, components[i]);
  }
}

const fileStamp = (root, file) => checkedPath(root, file, 'source');
const directoryId = root => checkedPath(root, root, 'directory');
const executableStat = (root, file) => checkedPath(root, file, 'executable');

function stamp(s) {
  return [s.dev, s.ino, s.size, s.mtimeNs, s.ctimeNs].join(':');
}

// isDirty is an editor flag. An external write may not have reached its file watcher yet.
// Compare only a user-selected, checked regular file, with a strict allocation/read limit.
async function savedStamp(v, root, { doc, version, file }) {
  const expected = await fileStamp(root, file);
  const handle = await open(file, O_RDONLY | O_NOFOLLOW | O_NONBLOCK);
  try {
    const st = await handle.stat({ bigint: true });
    if (!st.isFile() || stamp(st) !== expected) throw Error();
    const size = Number(st.size);
    const bytes = Buffer.alloc(size);
    for (let offset = 0; offset < size;) {
      const { bytesRead } = await handle.read(bytes, offset, size - offset, offset);
      if (bytesRead === 0) throw Error();
      offset += bytesRead;
    }
    const bom = bytes[0] === 0xef && bytes[1] === 0xbb && bytes[2] === 0xbf ? 3 : 0;
    if (bytes.includes(0) || doc.version !== version || doc.isClosed || doc.isDirty ||
        doc.offsetAt(new v.Position(doc.lineCount, 0)) > maxBytes) throw Error();
    const text = doc.getText();
    if (Buffer.byteLength(text, 'utf8') !== size - bom || !bytes.subarray(bom).equals(Buffer.from(text, 'utf8')) ||
        stamp(await handle.stat({ bigint: true })) !== expected) throw Error();
    return expected;
  } finally {
    await handle.close();
  }
}

module.exports = { candidates, fileStamp, savedStamp, localPath, directoryId, executableStat };
