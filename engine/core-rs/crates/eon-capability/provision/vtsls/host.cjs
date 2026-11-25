// In-process implementation of vtsls's TsServerProcessFactory port. No fork,
// worker, dynamic SDK/plugin loading, disk project or fallback to ts.sys.
"use strict";
const ts = require("typescript");
const path = require("node:path").posix;
const libs = new Map(Object.entries(require("./libs.json")));
const noop = () => {};
const logger = { close: noop, hasLevel: () => false, loggingEnabled: () => false,
  perftrc: noop, info: noop, startGroup: noop, endGroup: noop, msg: noop, getLogFileName: noop };

exports.factory = { fork(version, _args, kind) {
  if (kind !== "main" || version.tsServerPath !== "/eon-typescript/tsserver.js") {
    throw Error("unsupported vtsls session");
  }
  const opened = new Set();
  let receive, stopped = false;
  const host = {
    args: [], newLine: "\n", useCaseSensitiveFileNames: true,
    getCurrentDirectory: () => "/", getExecutingFilePath: () => version.tsServerPath,
    getEnvironmentVariable: () => "", resolvePath: p => path.resolve("/", p),
    realpath: p => p, fileExists: p => opened.has(p) || libs.has(p),
    readFile: p => libs.get(p),
    directoryExists: p => p === "/" || p === "/eon-typescript" ||
      [...opened].some(f => f.startsWith(p + "/")),
    getDirectories: () => [], readDirectory: () => [],
    writeFile() { throw Error("read-only"); }, createDirectory() { throw Error("read-only"); },
    write() { throw Error("not a stdio transport"); },
    watchFile: () => ({ close: noop }), watchDirectory: () => ({ close: noop }),
    setTimeout, clearTimeout, setImmediate, clearImmediate,
  };
  class Session extends ts.server.Session {
    parseMessage(request) { return request; }
    toStringMessage(request) { return JSON.stringify(request); }
    send(message) { if (!stopped) receive(message); }
    reloadMembership() { this.projectService.reloadProjects(); }
  }
  const session = new Session({
    host, logger, cancellationToken: ts.server.nullCancellationToken,
    typingsInstaller: ts.server.nullTypingsInstaller, byteLength: Buffer.byteLength, hrtime: process.hrtime,
    useSingleInferredProject: true, useInferredProjectPerProjectRoot: false, canUseEvents: true,
    allowLocalPluginLoads: false, globalPlugins: [], pluginProbeLocations: [],
  });
  return {
    onData(fn) { receive = fn; }, onError: noop, onExit: noop,
    kill() { stopped = true; opened.clear(); },
    write(request) {
      // Preserve the asynchronous boundary of the upstream transport (request
      // callbacks are registered by vtsls before the response is delivered).
      queueMicrotask(() => {
        if (stopped) return;
        const membershipChanged = request.command === "updateOpen" &&
          (request.arguments.openFiles?.length || request.arguments.closedFiles?.length);
        if (membershipChanged) {
          for (const f of request.arguments.openFiles ?? []) opened.add(f.file);
          for (const f of request.arguments.closedFiles ?? []) opened.delete(f);
        }
        session.onMessage(request);
        // There are no disk watchers to invalidate a cached import miss after
        // (re)open. Reload the bounded in-memory project, not its open texts.
        if (membershipChanged) session.reloadMembership();
      });
    },
  };
} };
