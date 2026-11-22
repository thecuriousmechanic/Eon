// vtsls pushes unversioned diagnostics. Queue fixed synchronous diagnostic
// requests through its own client so earlier buffered edits are flushed.
// Do not label old pushes as fresh or expose executeCommand over the wire.
"use strict";
const { fileURLToPath } = require("node:url");
exports.bind = (conn, service) => {
  conn.languages.diagnostics.on(async ({ textDocument }) => {
    const file = fileURLToPath(textDocument.uri);
    const items = [];
    for (const command of ["syntacticDiagnosticsSync", "semanticDiagnosticsSync"]) {
      const result = await service.executeCommand({
        command: "typescript.tsserverRequest",
        arguments: [command, { file, includeLinePosition: true }],
      });
      if (!result?.success || !Array.isArray(result.body)) throw Error("no TypeScript diagnostic report");
      for (const d of result.body) {
        const pos = p => ({ line: p.line - 1, character: p.offset - 1 });
        items.push({ range: { start: pos(d.startLocation), end: pos(d.endLocation) }, message: d.message });
      }
    }
    return { kind: "full", items };
  });
};
