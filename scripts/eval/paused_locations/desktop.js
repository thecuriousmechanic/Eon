'use strict';

const fs = require('node:fs');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const assert = require('node:assert/strict');
const { repo, workDirectory } = require('./host');
const { prepare } = require('./subject');

const allocations = process.argv[2] === '--verify-allocation-desktop';
assert.deepEqual(process.argv.slice(2), [allocations ? '--verify-allocation-desktop' : '--verify-disposable-desktop']);
const tools = path.join(repo, 'target/paused-ui-tools');
const work = workDirectory();
const home = path.join(work, 'home');
const project = path.join(home, 'project');
function json(file, data) {
  fs.mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
  fs.writeFileSync(file, JSON.stringify(data));
}
try {
  fs.mkdirSync(project, { recursive: true, mode: 0o700 });
  if (allocations) {
    require('./allocation_project').build(project, true);
    const config = { type: 'lldb-dap', request: 'launch', program: '${workspaceFolder}/allocation-subject',
      cwd: '${workspaceFolder}', disableASLR: false };
    json(path.join(project, '.vscode/launch.json'), { version: '0.2.0', configurations: [
      { ...config, name: 'Count once', eonAllocationCounts: true }, { ...config, name: 'Not opted in' },
    ] });
  } else {
    assert.deepEqual(prepare(project, true).lines, [28, 22]);
    fs.unlinkSync(path.join(project, 'main.rs'));
    json(path.join(project, '.vscode/launch.json'), {
      version: '0.2.0',
      configurations: [{ name: 'Fixed voice preview', type: 'lldb-dap', request: 'launch',
        program: '/home/ui/project/preview', cwd: '/home/ui/project', disableASLR: false }],
    });
  }
  json(path.join(home, 'profile/User/settings.json'), {
    'workbench.enableExperiments': false, 'workbench.startupEditor': 'none',
    'workbench.editor.enablePreview': false, 'workbench.colorTheme': 'Default Light Modern',
    'workbench.tips.enabled': false, 'window.dialogStyle': 'custom', 'window.zoomLevel': 1,
    'update.mode': 'none', 'extensions.autoUpdate': false, 'extensions.autoCheckUpdates': false,
    'extensions.ignoreRecommendations': true, 'telemetry.telemetryLevel': 'off', 'git.enabled': false,
    // No real workspace is trusted: the disposable instance can open only the fixed project.
    'security.workspace.trust.enabled': false,
    'lldb-dap.executable-path': '/usr/lib/llvm-18/bin/lldb-dap', 'lldb-dap.serverMode': false,
    'lldb-dap.captureSessionLogs': false, 'lldb-dap.disableASLR': false,
  });
  const ext = path.join(home, 'extensions');
  fs.mkdirSync(ext);
  fs.cpSync(path.join(repo, 'integrations/vscode-paused-locations'), path.join(ext, 'eon.eon-paused-locations-0.1.0'), { recursive: true });
  const vsix = path.join(tools, 'lldb.vsix');
  assert.equal(require('node:crypto').createHash('sha256').update(fs.readFileSync(vsix)).digest('hex'),
    'cd6bec2f77813c6bb010002044c6b231d1ec3615d2584c6fddd09988ade8b863');
  assert.equal(spawnSync('/usr/bin/unzip', ['-q', vsix, 'extension/*', '-d', ext]).status, 0);
  fs.renameSync(path.join(ext, 'extension'), path.join(ext, 'llvm-vs-code-extensions.lldb-dap-0.7.20260710'));
  fs.copyFileSync(path.join(__dirname, 'desktop_ui.py'), path.join(home, 'desktop_ui.py'));
  const driver = path.join(ext, 'eon-test.fixed-desktop-0.0.1');
  json(path.join(driver, 'package.json'), {
    name: 'fixed-desktop', publisher: 'eon-test', version: '0.0.1', engines: { vscode: '^1.93.0' },
    activationEvents: ['onStartupFinished'], main: './desktop_test.js',
  });
  fs.copyFileSync(path.join(__dirname, allocations ? 'allocation_desktop_test.js' : 'desktop_test.js'), path.join(driver, 'desktop_test.js'));
  fs.writeFileSync(path.join(home, 'sway.conf'), 'output * resolution 1440x1000\nxwayland disable\n');
  fs.mkdirSync(path.join(home, 'runtime'), { mode: 0o700 });
  const result = spawnSync('/usr/bin/podman', ['unshare', '/usr/bin/unshare', '--mount', '--pid', '--fork',
    '--kill-child=KILL', '--mount-proc', '--net', '--ipc', '/bin/sh', path.join(__dirname, 'desktop_ns.sh'), work, tools],
  { stdio: 'inherit', timeout: 145_000 });
  assert.equal(result.status, 0, 'disposable desktop did not verify both dialogs');
} finally {
  fs.rmSync(work, { recursive: true, force: true });
}
