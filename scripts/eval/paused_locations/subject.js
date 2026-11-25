'use strict';

const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const { spawnSync } = require('node:child_process');
const { createHash } = require('node:crypto');
const assert = require('node:assert/strict');
const { repo } = require('./host');

const project = path.join(repo, 'engine/core-rs/crates/eon-voice');
const llvm = '/usr/lib/llvm-18/bin';
const inputs = [
  ['src/endpoint.rs', '24c839b1f80898e6bb29e1abe2a81c27b588768b65974815b8e7558c6b6e8a22', '-> Option<Self> {\n        let text = text.trim().trim_end_matches('],
  ['src/pre_cognition.rs', '3fb0812fd2487460faa3c4abc7404c3e20d016e997573ad016d9bae9cf190a92', '&& let Some(hint) = ReflexHint::from_partial(text)'],
];

function checked(file, digest) {
  const st = fs.lstatSync(file);
  assert.ok(st.isFile() && st.size < 64 * 1024 && !(st.mode & 0o022));
  const bytes = fs.readFileSync(file);
  assert.equal(createHash('sha256').update(bytes).digest('hex'), digest);
  return bytes.toString('utf8');
}

function prepare(work, desktop = false) {
  const env = { HOME: work, TMPDIR: work, XDG_CACHE_HOME: work, PATH: '/usr/bin:/bin',
    LLDB_DEBUGSERVER_PATH: `${llvm}/lldb-server`, PYTHONNOUSERSITE: '1' };
  const files = [];
  const lines = [];
  fs.mkdirSync(path.join(work, 'src'), { mode: 0o700 });
  for (const [rel, digest, marker] of inputs) {
    const file = path.join(project, rel);
    const text = checked(file, digest);
    const offset = text.indexOf(marker);
    assert.ok(offset >= 0 && offset === text.lastIndexOf(marker));
    files.push([file, text]);
    lines.push(text.slice(0, offset + marker.length).split('\n').length);
    fs.writeFileSync(path.join(work, rel), text);
  }
  fs.writeFileSync(path.join(work, 'main.rs'), `
#[path = "src/endpoint.rs"] mod endpoint;
#[path = "src/pre_cognition.rs"] mod pre_cognition;
fn main() {
    use pre_cognition::IntentPreview;
    assert_eq!(IntentPreview::from_partial("how long"), Some(IntentPreview::Question));
}
`);
  const to = desktop ? '/home/ui/project' : project;
  assert.equal(spawnSync(path.join(os.homedir(), '.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc'),
    ['--edition=2024', '-g', '-C', 'opt-level=0', '-C', 'linker=/usr/bin/cc',
      '--remap-path-prefix', `${path.join(work, 'src')}=${path.join(to, 'src')}`, path.join(work, 'main.rs'), '-o', 'preview'],
    { cwd: work, env, stdio: 'ignore', timeout: 30_000 }).status, 0);
  return { env, files, lines };
}

module.exports = { prepare, checked, inputs, llvm, project };
