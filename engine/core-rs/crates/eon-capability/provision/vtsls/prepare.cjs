// Adapt only the host-dependent ports of the pinned upstream distribution.
// The vtsls LSP providers, editor synchronization and TS compiler are upstream.
"use strict";
const fs = require("node:fs");
const path = require("node:path");
function adapt(file, changes) {
  let text = fs.readFileSync(file, "utf8");
  for (const [from, to] of changes) {
    if (text.split(from).length !== 2) throw Error(`unexpected upstream source: ${from}`);
    text = text.replace(from, () => to);
  }
  fs.writeFileSync(file, text);
}
const libraries = {};
const lib = "node_modules/typescript/lib";
for (const name of fs.readdirSync(lib).filter(s => /^lib(?:\.[\w.-]+)?\.d\.ts$/.test(s))) {
  libraries["/eon-typescript/" + name] = fs.readFileSync(path.join(lib, name), "utf8");
}
fs.writeFileSync("libs.json", JSON.stringify(libraries));
adapt("node_modules/@vtsls/language-service/dist/index.js", [
  ["const versionProvider = new DiskTypeScriptVersionProvider();", `
    const sdk = new TypeScriptVersion("bundled", "/eon-typescript/tsserver.js", API.fromVersionString("5.9.3"));
    const versionProvider = { defaultVersion: sdk, bundledVersion: sdk, localVersions: [], updateConfiguration() {} };`],
  ["req = (0, import_module.createRequire)(importMetaUrl);", ""],
  ["processFactory: new ElectronServiceProcessFactory(),", 'processFactory: require("../../../../host.cjs").factory,'],
  ["cancellerFactory: nodeRequestCancellerFactory,", "cancellerFactory: { create: () => ({ tryCancelOngoingRequest: () => false }) },"],
  ["fs8.rmSync(instanceTempDir.value, { recursive: true, force: true });", ""],
]);
adapt("node_modules/@vtsls/language-server/dist/main.js", [
  ["await service.initialize({});", `await service.initialize({ typescript: { tsserver: { useSyntaxServer: "never" }, disableAutomaticTypeAcquisition: true } });`],
  ["const capabilities = getTsLspDefaultCapabilities();", `const capabilities = getTsLspDefaultCapabilities();
   capabilities.diagnosticProvider = { interFileDependencies: true, workspaceDiagnostics: false };
   delete capabilities.executeCommandProvider;`],
  ["conn.onExecuteCommand(safeRun(service.executeCommand, null));", `require("../../../../diagnostics.cjs").bind(conn, service);`],
]);
fs.writeFileSync("entry.cjs", `
if (process.argv.length !== 3 || process.argv[2] !== "--stdio") process.exit(2);
require("./node_modules/@vtsls/language-server/dist/main.js");`);
fs.writeFileSync("sea.json", JSON.stringify({
  main: "main.cjs", output: "sea.blob", disableExperimentalSEAWarning: true,
  useSnapshot: false, useCodeCache: false, execArgvExtension: "none",
  execArgv: ["--jitless", "--disallow-code-generation-from-strings", "--no-addons",
    "--disable-sigusr1", "--disable-wasm-trap-handler", "--v8-pool-size=1", "--max-old-space-size=192"],
}));
