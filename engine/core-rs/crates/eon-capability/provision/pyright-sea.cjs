// Build input generator, run by the operator, never by the LSP client.
// Only the three hash-checked upstream webpack chunks become executable JS.
"use strict";
const fs = require("node:fs");
const path = require("node:path");
const { createHash } = require("node:crypto");
const [dist, out] = process.argv.slice(2);
const chunks = {
  "vendor.js": "1c691bf9e3fa954dec751ace203beb5591b46a73e3848af5d1ee011d62540798",
  "pyright-internal.js": "41fb260ad72f2a16c5657e1f2fe837104d7529255d0fcd955f48432b276e7cae",
  "pyright-langserver.js": "467344c79457ec74f7f7b6cb0f52c3c5ebc7ea77bf53edeb6d2dc6217687b507",
};
function source(name) {
  const bytes = fs.readFileSync(path.join(dist, name));
  if (createHash("sha256").update(bytes).digest("hex") !== chunks[name])
    throw Error(`expected pyright 1.1.414 ${name}`);
  return bytes.toString("utf8");
}
// SEA's require only loads built-ins. Do not add a disk-backed require,
// project module resolver, dynamic compilation, or caller-selected entrypoint.
let main = `
"use strict";
if (process.argv.length !== 3 || process.argv[2] !== "--stdio") process.exit(2);
global.__rootDirectory = require("node:path").dirname(process.execPath);
if (process.cwd() !== global.__rootDirectory) throw Error("expected fixed installation cwd");
// Pyright sets its startup cwd to its bundle directory. The trusted parent
// has already done exactly that. No subsequent cwd mutation is supported.
process.chdir = dir => {
  if (dir !== global.__rootDirectory) throw Error("unsupported chdir");
};
const chunks = Object.create(null);
`;
for (const name of ["vendor.js", "pyright-internal.js"]) {
  main += `chunks["./${name}"] = {}; (function(exports) {\n${source(name)}\n})(chunks["./${name}"]);\n`;
}
main += `(function(require) {\n${source("pyright-langserver.js")}\n})(name => chunks[name] ?? require(name));\n`;
fs.writeFileSync(path.join(out, "main.cjs"), main, { flag: "wx" });
fs.writeFileSync(path.join(out, "sea.json"), JSON.stringify({
  main: path.resolve(out, "main.cjs"), output: path.resolve(out, "sea.blob"),
  disableExperimentalSEAWarning: true, useSnapshot: false, useCodeCache: false,
  execArgvExtension: "none",
  execArgv: ["--jitless", "--disallow-code-generation-from-strings", "--no-addons",
    "--disable-sigusr1", "--disable-wasm-trap-handler", "--v8-pool-size=1", "--max-old-space-size=192"],
}), { flag: "wx" });
