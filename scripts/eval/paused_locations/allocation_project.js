'use strict';

const fs = require('node:fs');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const assert = require('node:assert/strict');
const { repo, editor } = require('./host');
const { llvm } = require('./subject');
const { register } = require('../../../integrations/vscode-paused-locations/allocations');

function build(root, desktop = false) {
  const env = { HOME: root, TMPDIR: root, PATH: '/usr/bin:/bin', LLDB_DEBUGSERVER_PATH: `${llvm}/lldb-server`,
    PYTHONNOUSERSITE: '1', XDG_CACHE_HOME: root };
  const file = path.join(root, 'allocation_subject.c');
  fs.copyFileSync(path.join(__dirname, 'allocation_subject.c'), file);
  const source = fs.readFileSync(file, 'utf8');
  const line = source.split('\n').findIndex(l => l.includes('/* source breakpoint */')) + 1;
  assert.ok(line);
  const program = path.join(root, 'allocation-subject');
  assert.equal(spawnSync(`${llvm}/clang`, ['--no-default-config', '--ld-path=/usr/bin/ld', '-std=c11', '-g',
    ...(desktop ? [`-fdebug-prefix-map=${root}=/home/ui/project`] : []),
    '-O0', '-fno-builtin', '-Wall', '-Wextra', '-Werror', file,
    path.join(repo, 'integrations/vscode-paused-locations/project_counts.c'),
    '-Wl,--wrap=malloc,--wrap=calloc,--wrap=realloc,--wrap=free', '-o', program],
  { cwd: root, env, stdio: 'inherit', timeout: 30_000 }).status, 0);
  return { env, file, source, program, line };
}

function host(root, program) {
  const h = editor(root, [], () => assert.fail('the allocation command sent a DAP request'));
  const { v, session } = h;
  const events = {};
  v.debug.registerDebugConfigurationProvider = (type, p) => {
    assert.equal(type, 'lldb-dap');
    events.resolve = (...args) => p.resolveDebugConfigurationWithSubstitutedVariables(h.folder, ...args);
  };
  v.debug.onDidStartDebugSession = cb => { events.start = cb; };
  v.debug.onDidTerminateDebugSession = cb => { events.end = cb; };
  h.prompts = [];
  v.window.showInformationMessage = async (_, { detail, modal }, ...actions) => {
    assert.equal(modal, true);
    if (!actions.length) { h.messages.push(detail); return; }
    h.prompts.push(...actions);
    return h.choose?.(actions[0]);
  };
  h.choose = action => action;
  session.type = 'lldb-dap';
  const counts = register(v);
  assert.equal(h.name, 'eon.showProjectAllocationCounts');
  return Object.assign(h, events, { close: counts.close, config: { type: session.type, name: 'Local project',
    request: 'launch', cwd: root, program, disableASLR: false, eonAllocationCounts: true } });
}

const expected = (malloc, calloc, realloc, free) =>
  `Successful malloc calls: ${malloc}\nSuccessful calloc calls: ${calloc}\nSuccessful positive-size realloc calls: ${realloc}\nNon-null free calls: ${free}`;

module.exports = { build, host, expected };
