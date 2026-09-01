import { after, before, test } from "node:test";
import assert from "node:assert/strict";
import * as http2 from "node:http2";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { ConnectRouter } from "@connectrpc/connect";
import { connectNodeAdapter } from "@connectrpc/connect-node";
import { TymuxService } from "../gen/tymux/v1/tymux_pb.js";
import { createUdsGrpcTransport } from "../examples/client.js";
import { attachListeningDiagnostic, diagLog, diagLogPaths, logListenError, startListenHeartbeat } from "./listen-diagnostics.js";

// Epic 8.1 Story 8.1.1's spike, kept as an automated test rather than a
// throwaway script (plan.md explicitly allows either): confirms
// createGrpcTransport's `nodeOptions.createConnection` pass-through
// actually reaches Node's `http2.connect()` and completes a real RPC over
// a Unix domain socket -- build-vs-buy.md §4's flagged, previously
// end-to-end-unverified risk.
//
// This uses a real connect-node h2c server (connectNodeAdapter +
// http2.createServer) bound to a Unix socket rather than a live tymuxd,
// since crates/tymuxd's UDS-listener wiring (Epic 2.2) is landing
// concurrently on this branch and this phase's TS work does not depend on
// it -- see Task 8.1.1a's note. The mechanism under test
// (createConnection's pass-through into http2.connect()) is entirely
// Node-side and identical regardless of what's on the other end of the
// socket.
//
// Result: the pass-through works exactly as documented, so
// createUdsGrpcTransport (Story 8.1.2, in examples/client.ts) uses it
// directly -- the createConnectTransport/custom-Agent fallback described
// in build-vs-buy.md §4 was not needed.

let server: http2.Http2Server;
let socketPath: string;
let stateDir: string;

before(async () => {
  const DIAG = "uds-transport:before";
  stateDir = mkdtempSync(join(tmpdir(), "tymux-ts-uds-spike-"));
  socketPath = join(stateDir, "spike.sock");
  diagLogPaths(DIAG, stateDir, socketPath);
  const routes = (router: ConnectRouter) => {
    router.service(TymuxService, {
      listSessions: () => ({ sessions: [] }),
    });
  };
  server = http2.createServer(connectNodeAdapter({ routes }));
  attachListeningDiagnostic(server, DIAG);
  const stopHeartbeat = startListenHeartbeat(DIAG);
  try {
    await new Promise<void>((resolve, reject) => {
      server.once("error", (err: NodeJS.ErrnoException) => {
        logListenError(DIAG, err);
        reject(err);
      });
      diagLog(DIAG, `calling .listen(${JSON.stringify(socketPath)}, callback)`);
      server.listen(socketPath, () => {
        diagLog(DIAG, "listen() callback fired");
        resolve();
      });
    });
  } finally {
    stopHeartbeat();
  }
});

after(async () => {
  await new Promise<void>((resolve) => server.close(() => resolve()));
  rmSync(stateDir, { recursive: true, force: true });
});

test("createUdsGrpcTransport round-trips listSessions over a real Unix domain socket", async () => {
  const client = createUdsGrpcTransport(socketPath);
  const response = await client.listSessions({});
  assert.deepEqual(response.sessions, []);
});
