import type * as http2 from "node:http2";

// Round-3 instrumentation for the CI-only http2/UDS `.listen()` hang under
// investigation (see integration.test.ts's "dials the resolved Unix socket
// first" test and uds-transport.test.ts's before() hook). Two prior fix
// attempts -- a timeout+serialization safety net, then wiring `.once("error",
// ...)` before `.listen()` (commit c0eb021) -- did not resolve it, and
// critically the error listener never fired either: CI just times out with
// zero diagnostic output. This module exists to make the NEXT CI run's log
// tell us exactly where execution stops. It is purely additive -- it never
// replaces the existing resolve/error wiring at each call site.
//
// Every write goes through process.stdout.write, never console.log/error:
// console.log can buffer under some conditions, and several of these test
// files assert on an exact count of captured console.error calls -- routing
// diagnostics through console would silently break those assertions.
export function diagLog(label: string, message: string): void {
  process.stdout.write(`[diag:${label}] ${message}\n`);
}

// Logs a freshly-created stateDir/socketPath pair and their string lengths.
// Rules out a subtle path-length issue distinct from the SUN_LEN bound
// already enforced elsewhere (Unix's sockaddr_un.sun_path is capped at 108
// bytes on Linux; mkdtemp's system tmp prefix plus a random suffix could in
// principle push a path close to that on some runners).
export function diagLogPaths(label: string, stateDir: string, socketPath: string): void {
  diagLog(label, `stateDir=${JSON.stringify(stateDir)} length=${stateDir.length}`);
  diagLog(label, `socketPath=${JSON.stringify(socketPath)} length=${socketPath.length}`);
}

// Attaches a 'listening' event listener IN ADDITION to whatever
// resolve/error wiring the caller already has on `.listen()` -- this tells
// us whether the event fires even if something about the callback itself
// (the second argument to .listen()) is broken.
export function attachListeningDiagnostic(server: http2.Http2Server, label: string): void {
  server.on("listening", () => {
    diagLog(label, "'listening' event fired");
  });
}

// Call from inside the test's EXISTING 'error' listener (do not replace that
// listener -- just log through this first, then still reject/handle as
// before). Logs the full error object via Object.getOwnPropertyNames, not
// just err.message, plus err.stack.
export function logListenError(label: string, err: NodeJS.ErrnoException): void {
  diagLog(label, `'error' event fired: ${err.stack ?? String(err)}`);
  diagLog(label, `full error object: ${JSON.stringify(err, Object.getOwnPropertyNames(err))}`);
}

// Starts a 3-second heartbeat proving the process/event loop is alive and
// only the .listen() operation is stuck, rather than the whole process being
// blocked. Returns a function that clears it -- call that unconditionally
// (in a finally) once the listen has settled one way or the other.
export function startListenHeartbeat(label: string): () => void {
  const start = Date.now();
  const heartbeat = setInterval(() => {
    diagLog(label, `still waiting on listen(), ${((Date.now() - start) / 1000).toFixed(0)}s elapsed`);
  }, 3000);
  return () => clearInterval(heartbeat);
}
