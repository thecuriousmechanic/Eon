'use strict';

// Test stand-in for the editor, not for the filesystem or for the native test's adapter.
const fs = require('node:fs');
const path = require('node:path');
const { randomUUID } = require('node:crypto');
const { register } = require('../../../integrations/vscode-paused-locations/command');

const repo = path.resolve(__dirname, '../../..');
const uri = fsPath => ({ scheme: 'file', fsPath, authority: '', query: '', fragment: '' });

function workDirectory() {
  const base = path.join(repo, 'target');
  fs.mkdirSync(base, { recursive: true, mode: 0o700 });
  for (let dir = base;; dir = path.dirname(dir)) {
    const st = fs.lstatSync(dir);
    if (!st.isDirectory() || (st.mode & 0o022) || ![0, process.geteuid()].includes(st.uid)) throw Error();
    if (dir === '/') break;
  }
  const work = path.join(base, `paused-project-${randomUUID()}`);
  fs.mkdirSync(work, { mode: 0o700 });
  return work;
}

function editor(root, files, customRequest, threadId = 1, frameId = 0) {
  class DebugStackFrame {}
  class TabInputText {}
  const folder = { uri: uri(root) };
  const docs = files.map(([file, text]) => ({
    uri: uri(file), lineCount: text.split('\n').length, offsetAt: () => text.length, getText: () => text,
    version: 1, isDirty: false, isClosed: false, isUntitled: false,
  }));
  const tabs = docs.map(doc => ({ input: Object.assign(new TabInputText(), { uri: doc.uri }), isDirty: false }));
  const session = { workspaceFolder: folder, customRequest };
  const frame = Object.assign(new DebugStackFrame(), { session, threadId, frameId });
  const h = { messages: [], offered: [], pick: () => undefined, command: undefined };
  const v = {
    DebugStackFrame, TabInputText, Position: class {}, UIKind: { Desktop: 1 }, env: { uiKind: 1 },
    debug: { activeDebugSession: session, activeStackItem: frame },
    commands: { registerCommand(name, fn) { h.name = name; h.command = fn; return { dispose() {} }; } },
    window: {
      tabGroups: { all: [{ tabs }] },
      async showQuickPick(items, options) {
        if (!options.canPickMany) throw Error();
        h.offered.push([...items]);
        return h.pick(items);
      },
      async showInformationMessage(_, { modal, detail }) {
        if (!modal) throw Error();
        h.messages.push(detail);
      },
    },
    workspace: { isTrusted: true, workspaceFolders: [folder], textDocuments: docs, getWorkspaceFolder: () => folder },
  };
  register(v);
  return Object.assign(h, { v, docs, tabs, session, frame, folder });
}

module.exports = { repo, workDirectory, editor };
