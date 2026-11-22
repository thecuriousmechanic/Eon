'use strict';

const { candidates, fileStamp, savedStamp, localPath } = require('./sources');

const command = 'eon.showPausedProjectLocations';
const levels = 12;
const unavailable = 'No locations shown. Use a trusted local folder, keep the selected sources saved and open, and focus a paused call frame.';

function focus(v) {
  if (process.platform !== 'linux' || v.env.uiKind !== v.UIKind.Desktop || v.env.remoteName ||
      !v.workspace.isTrusted) return;
  const session = v.debug.activeDebugSession;
  const frame = v.debug.activeStackItem;
  const root = localPath(session?.workspaceFolder?.uri);
  if (root && root !== '/' && v.workspace.workspaceFolders?.some(f => localPath(f.uri) === root) &&
      frame instanceof v.DebugStackFrame && frame.session === session &&
      Number.isSafeInteger(frame.frameId) && frame.frameId >= 0 &&
      Number.isSafeInteger(frame.threadId) && frame.threadId > 0 && frame.threadId <= 0x7fffffff) {
    return { session, frame, root };
  }
}

function locations(body, selected) {
  const frames = body?.stackFrames;
  if (!Array.isArray(frames) || frames.length > levels) throw Error();
  const rows = [];
  for (const frame of frames) {
    const source = frame?.source;
    if (!source || (source.sourceReference !== undefined && source.sourceReference !== 0) ||
        typeof source.path !== 'string' || source.path.length > 4096) continue;
    const item = selected.find(s => s.file === source.path);
    if (item && Number.isSafeInteger(frame.line) && frame.line >= 1 && frame.line <= item.doc.lineCount) {
      rows.push({ item, line: frame.line });
    }
  }
  return rows;
}

// Editor configuration, not the reason for the stop. Never send an expression to the adapter.
function describe(v, rows) {
  const configured = new Set();
  const points = v.debug.breakpoints;
  if (Array.isArray(points) && points.length <= 256) {
    for (const point of points) {
      if (!(point instanceof v.SourceBreakpoint) || !point.enabled) continue;
      const file = localPath(point.location.uri);
      const line = point.location.range.start.line + 1;
      if (!rows.some(row => row.item.file === file && row.line === line)) continue;
      if (!point.logMessage && (point.condition || point.hitCondition)) configured.add(`${file}:${line}`);
    }
  }
  return rows.map(({ item, line }) => `${item.label}:${line}` +
    (configured.has(`${item.file}:${line}`) ? ' [conditional breakpoint configured]' : ''));
}

function register(v) {
  let busy = false;
  const pending = new WeakSet();
  const tell = detail => v.window.showInformationMessage('Paused project locations', { modal: true, detail });
  return v.commands.registerCommand(command, async (...args) => {
    if (args.length || busy) return;
    busy = true;
    try {
      const start = focus(v);
      if (!start || pending.has(start.session)) throw Error();
      const { session, frame, root } = start;
      const options = candidates(v, root);
      if (!options.size) {
        await tell('Open and save source files in the paused session’s workspace folder first.');
        return;
      }
      const picked = await v.window.showQuickPick([...options.keys()].sort(), {
        title: 'Show locations once for my local debug session',
        placeHolder: 'Check 1–8 open source files you own; Enter to allow, Escape to cancel.',
        canPickMany: true,
      });
      if (!picked?.length) return;
      if (picked.length > 8 || new Set(picked).size !== picked.length || !picked.every(p => options.has(p))) {
        await tell('Select at most eight listed sources.');
        return;
      }
      const selected = picked.map(p => options.get(p));
      function checkEditor() {
        const now = focus(v);
        if (now?.frame !== frame || now.session !== session || now.root !== root) throw Error();
        const open = candidates(v, root);
        for (const item of selected) {
          const current = open.get(item.label);
          if (current?.doc !== item.doc || current.version !== item.version) throw Error();
        }
      }
      checkEditor();
      const before = [];
      for (const item of selected) before.push(await savedStamp(v, root, item));
      checkEditor();

      // customRequest isn't cancellable. On timeout discard the reply and don't queue
      // another request on this session until the original settles.
      pending.add(session);
      let timer;
      let rows;
      try {
        const request = Promise.resolve().then(() => {
          checkEditor();
          return session.customRequest('stackTrace', { threadId: frame.threadId, startFrame: 0, levels });
        }).finally(() => pending.delete(session));
        const body = await Promise.race([
          request,
          new Promise((_, reject) => { timer = setTimeout(() => reject(Error()), 3000); }),
        ]);
        rows = locations(body, selected);
      } finally {
        clearTimeout(timer);
      }
      const after = await Promise.all(selected.map(s => fileStamp(root, s.file)));
      checkEditor();
      if (!after.every((s, i) => s === before[i])) throw Error();
      await tell(rows.length ? describe(v, rows).join('\n') : 'No selected file matched the first 12 frames.');
    } catch {
      await tell(unavailable); // Never surface debugger messages, exceptions, paths or source text.
    } finally {
      busy = false;
    }
  });
}

module.exports = { register, focus };
