'use strict';

// Runs in the real VS Code extension test host. Set up exactly one fixed
// breakpoint/configuration, then leave command invocation and consent to the GUI.
const v = require('vscode');
const fs = require('node:fs/promises');
const assert = require('node:assert/strict');
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const project = '/home/ui/project';

async function until(check) {
  for (let i = 0; i < 300; i++) {
    if (await check()) return;
    await delay(200);
  }
  throw Error('desktop stage timed out');
}

exports.activate = async () => {
  assert.equal(process.env.HOME, '/home/ui');
  await fs.writeFile('/home/ui/runtime/phase', 'host-ready');
  const [folder] = v.workspace.workspaceFolders;
  assert.equal(folder.uri.fsPath, project);
  assert.ok(v.workspace.isTrusted);
  for (const file of ['pre_cognition.rs', 'endpoint.rs']) {
    const doc = await v.workspace.openTextDocument(v.Uri.file(`${project}/src/${file}`));
    await v.window.showTextDocument(doc, { preview: false });
  }
  const doc = v.window.activeTextEditor.document;
  assert.ok(doc.lineAt(27).text.includes('let text = text.trim().trim_end_matches('));
  // Model a user-configured first-hit stop via the real editor breakpoint model.
  v.debug.addBreakpoints([new v.SourceBreakpoint(new v.Location(doc.uri, new v.Position(27, 0)), true, undefined, '1')]);
  await fs.writeFile('/home/ui/runtime/phase', 'start-fixed-session');
  assert.ok(await v.debug.startDebugging(folder, 'Fixed voice preview'));
  await fs.writeFile('/home/ui/runtime/phase', 'wait-for-frame');
  await until(() => v.debug.activeStackItem instanceof v.DebugStackFrame);
  const frame = v.debug.activeStackItem;
  assert.equal(frame.session, v.debug.activeDebugSession);
  assert.equal(frame.session.workspaceFolder.uri.fsPath, project);
  await fs.writeFile('/home/ui/runtime/phase', 'real-frame');
  for (const stage of ['both', 'caller']) {
    assert.equal(v.debug.activeStackItem, frame);
    await v.commands.executeCommand('workbench.action.quickOpen', '>EON: Show Paused Project Locations');
    await until(async () => (await fs.stat(`/home/ui/runtime/${stage}.palette`).catch(() => undefined))?.isFile());
    await v.commands.executeCommand('workbench.action.acceptSelectedQuickOpenItem');
    await until(async () => (await fs.stat(`/home/ui/runtime/${stage}.picker`).catch(() => undefined))?.isFile());
    // VS Code 1.93 clears row focus when entering checkbox mode.
    await v.commands.executeCommand('workbench.action.quickOpenSelectNext');
    if (stage === 'both') await v.commands.executeCommand('workbench.action.quickPickManyToggle');
    await v.commands.executeCommand('workbench.action.quickOpenSelectNext');
    await v.commands.executeCommand('workbench.action.quickPickManyToggle');
    await v.commands.executeCommand('workbench.action.acceptSelectedQuickOpenItem');
    await fs.writeFile('/home/ui/runtime/phase', `selected-${stage}`);
    await until(async () => (await fs.stat(`/home/ui/runtime/${stage}.seen`).catch(() => undefined))?.isFile());
  }
  assert.equal(v.debug.activeStackItem, frame);
  await v.commands.executeCommand('workbench.action.debug.continue');
  await until(() => !v.debug.activeDebugSession);
  await fs.writeFile('/home/ui/runtime/done', 'done');
};
