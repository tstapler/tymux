import { test } from "node:test";
import * as http2 from "node:http2";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  attachListeningDiagnostic,
  diagLog,
  diagLogPaths,
  logListenError,
  startListenHeartbeat,
} from "./listen-diagnostics.js";

// Round-3 CI-only hang investigation (see integration.test.ts's "dials the
// resolved Unix socket first" test and uds-transport.test.ts's before()
// hook -- both hang on GH Actions ubuntu-latest with neither the success
// callback nor an 'error' listener ever firing, but never reproduce
// locally). Both of those tests layer a connect-node adapter + routing on
// top of http2.createServer -- this test strips all of that away to isolate
// whether the hang is about `http2.createServer` + Unix sockets in general
// on this runner, or something specific to connectNodeAdapter/routing. It
// does nothing but bind the bare minimum http2 server to a fresh Unix
// socket, and reports success or a detailed failure via the shared
// listen-diagnostics helpers.
test("bare http2.createServer listens on a Unix socket with no connect-node adapter involved", async () => {
  const DIAG = "http2-uds-diagnostic";
  const stateDir = mkdtempSync(join(tmpdir(), "tymux-ts-http2-uds-diag-"));
  const socketPath = join(stateDir, "diag.sock");
  diagLogPaths(DIAG, stateDir, socketPath);

  const server = http2.createServer((_req, res) => {
    res.end();
  });
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
    diagLog(DIAG, "bare http2 server bound successfully -- closing");
  } finally {
    stopHeartbeat();
    await new Promise<void>((resolve) => server.close(() => resolve()));
    rmSync(stateDir, { recursive: true, force: true });
  }
});
