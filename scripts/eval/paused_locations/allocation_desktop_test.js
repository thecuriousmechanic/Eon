'use strict';

// Test-only driver in the disposable workbench; no replacement UI, adapter or
// launch resolver. The other process reads/clicks the actual rendered dialogs.
const v = require('vscode');
const fs = require('node:fs/promises');
const assert = require('node:assert/strict');
const root = '/home/ui/project';
const runtime = '/home/ui/runtime';
const phase = text => fs.writeFile(`${runtime}/phase`, text);
async function until(check) {
  for (let i = 0; i < 300; i++) {
    if (await check()) return;
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  throw Error('fixed desktop stage timed out');
}

exports.activate = async () => {
  const [folder] = v.workspace.workspaceFolders;
  assert.equal(folder.uri.fsPath, root);
  assert.ok(v.workspace.isTrusted);
  const doc = await v.workspace.openTextDocument(v.Uri.file(`${root}/allocation_subject.c`));
  await v.window.showTextDocument(doc);
  const line = doc.getText().split('\n').findIndex(l => l.includes('/* source breakpoint */'));
  assert.ok(line > 0);
  v.debug.addBreakpoints([new v.SourceBreakpoint(new v.Location(doc.uri, new v.Position(line, 0)))]);
  for (const optIn of [true, false]) {
    await phase(optIn ? 'start opted-in launch' : 'start opt-out launch');
    assert.ok(await v.debug.startDebugging(folder, optIn ? 'Count once' : 'Not opted in'));
    await until(() => v.debug.activeStackItem instanceof v.DebugStackFrame);
    const session = v.debug.activeDebugSession;
    const c = session.configuration;
    assert.equal(c.cwd, root);
    assert.equal(c.program, `${root}/allocation-subject`);
    if (optIn) {
      assert.deepEqual(Object.keys(c).sort(), ['__configurationTarget', 'cwd', 'disableASLR', 'env',
        'eonAllocationCounts', 'name', 'program', 'request', 'type']);
      assert.ok(Array.isArray(c.env));
      assert.equal(c.env.length, 1);
      assert.match(c.env[0], /^EON_PROJECT_ALLOC=[0-9a-f]{32}$/);
      await fs.writeFile(`${runtime}/profile`, Object.keys(c).sort().join(', ')); // Keys only.
    } else {
      assert.equal(c.env, undefined);
    }
    for (const stage of optIn ? ['first', 'second'] : ['negative']) {
      const frame = v.debug.activeStackItem;
      assert.ok(frame instanceof v.DebugStackFrame && frame.session === session);
      await phase(stage);
      await v.commands.executeCommand('workbench.action.quickOpen', '>EON: Show Project Allocation Counts');
      await until(async () => (await fs.stat(`${runtime}/${stage}.palette`).catch(() => undefined))?.isFile());
      await v.commands.executeCommand('workbench.action.acceptSelectedQuickOpenItem');
      await until(async () => (await fs.stat(`${runtime}/${stage}.seen`).catch(() => undefined))?.isFile());
      assert.equal(v.debug.activeStackItem, frame);
      if (stage === 'negative') {
        // This is the same normal two-stop program; finish it without observing its second stop.
        v.debug.removeBreakpoints(v.debug.breakpoints);
      }
      await v.commands.executeCommand('workbench.action.debug.continue');
      if (stage === 'first') await until(() => v.debug.activeStackItem instanceof v.DebugStackFrame &&
        v.debug.activeStackItem !== frame);
    }
    await until(() => !v.debug.activeDebugSession);
  }
  await fs.writeFile(`${runtime}/done`, 'done');
};
