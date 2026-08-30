import { after, before, test } from "node:test";
import assert from "node:assert/strict";
import * as net from "node:net";
import * as http2 from "node:http2";
import { spawn } from "node:child_process";
import { chmodSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Code, ConnectError, createClient, type ConnectRouter } from "@connectrpc/connect";
import { connectNodeAdapter, createGrpcTransport } from "@connectrpc/connect-node";
import { TymuxService, type AttachRequest } from "../gen/tymux/v1/tymux_pb.js";
import { runAttachDemo } from "../examples/attach.js";
import { tymuxClient } from "../examples/client.js";
import { capturePane } from "../examples/capture-pane.js";
import { startDaemon, type TestDaemon } from "./daemon.js";

let daemon: TestDaemon;
let client: ReturnType<typeof createClient<typeof TymuxService>>;

before(async () => {
  daemon = await startDaemon();
  client = createClient(TymuxService, createGrpcTransport({ baseUrl: daemon.addr }));
});

after(() => {
  daemon.stop();
});

// Story 7.2 AC1: unary RPC round-trip end-to-end through the generated client.
test("listSessions reflects a session created via createSession", async () => {
  const created = await client.createSession({ name: "ts-integration", command: "" });
  const listed = await client.listSessions({});
  const found = listed.sessions.find((s) => s.id === created.id);
  assert.ok(found, "created session should appear in listSessions");
  assert.equal(found?.name, "ts-integration");
});

// Story 7.3 AC1/AC2: Attach's bidi stream carries real command execution,
// full-cancellation is the detach contract (Epic 2 Story 2.3), and
// CapturePane independently proves the pane's screen state — all three of
// requirements.md's named RPCs demonstrated from a non-Rust client.
test("attach executes input and full-cancellation leaves the pane live; capturePane reflects its screen", async () => {
  const session = await client.createSession({ name: "ts-attach-integration", command: "" });
  const node = session.windows[0]?.layout?.node;
  assert.equal(node?.case, "pane", "a fresh session's window should be a single-pane leaf");
  if (node?.case !== "pane") throw new Error("unreachable");
  const paneId = node.value.id;

  const { output } = await runAttachDemo(paneId, { baseUrl: daemon.addr });
  assert.ok(output.includes("tymux-ts-marker-output"), "attach should observe the command's real output");

  const afterList = await client.listSessions({});
  const stillLive = afterList.sessions.find((s) => s.id === session.id);
  assert.equal(stillLive?.liveness, 1 /* LIVENESS_LIVE */, "full-cancellation must not kill the pane's process");

  const snapshot = await capturePane(paneId, daemon.addr);
  const screenText = snapshot.grid.map((row) => row.cells.map((cell) => cell.text).join("")).join("\n");
  assert.ok(
    screenText.includes("tymux-ts-marker-output"),
    "capturePane snapshot should reflect the pane's actual current screen",
  );
});

// --- Epic 3.2: TS client bearer-token auth against a live, token-gated
// daemon. Each test spins up its own token-gated daemon (distinct from the
// no-auth `daemon` shared above) since auth enforcement only kicks in on a
// non-loopback bind (`startDaemon({ token })` binds 0.0.0.0 for this
// reason — see daemon.ts's doc comment) and these tests would otherwise
// interfere with the no-token tests sharing the module-level `daemon`.

const AUTH_TOKEN = "s3cr3t";

function assertUnauthenticated(err: unknown) {
  assert.ok(err instanceof ConnectError, `expected a ConnectError, got ${String(err)}`);
  assert.equal((err as ConnectError).code, Code.Unauthenticated);
  return true;
}

// Story 3.2.1 AC1 / Task 3.2.1c: missing/wrong token rejected on a unary call.
test("listSessions rejects a missing/wrong token", async () => {
  const tokenDaemon = await startDaemon({ token: AUTH_TOKEN });
  try {
    const unauthedClient = await tymuxClient(tokenDaemon.addr);
    await assert.rejects(() => unauthedClient.listSessions({}), assertUnauthenticated);
  } finally {
    tokenDaemon.stop();
  }
});

// Story 3.2.1 AC2 / Task 3.2.1c: correct token succeeds on a unary call.
test("listSessions succeeds with the correct token", async () => {
  const tokenDaemon = await startDaemon({ token: AUTH_TOKEN });
  try {
    const authedClient = await tymuxClient(tokenDaemon.addr, AUTH_TOKEN);
    const listed = await authedClient.listSessions({});
    assert.ok(Array.isArray(listed.sessions), "listSessions should succeed and return a sessions array");
  } finally {
    tokenDaemon.stop();
  }
});

// Story 3.2.1 AC3 / Task 3.2.1d: missing/wrong token rejected on the
// streaming Attach RPC specifically — the auth interceptor applies
// uniformly to unary and streaming calls, so a bogus pane ID is enough to
// prove rejection happens before any pane lookup.
test("attach rejects a missing/wrong token", async () => {
  const tokenDaemon = await startDaemon({ token: AUTH_TOKEN });
  try {
    await assert.rejects(
      () =>
        withTimeout(
          runAttachDemo("nonexistent-pane-id", { baseUrl: tokenDaemon.addr }),
          5_000,
          "attach rejects a missing/wrong token",
        ),
      assertUnauthenticated,
    );
  } finally {
    tokenDaemon.stop();
  }
});

// Story 3.2.1 AC4 / Task 3.2.1d: correct token succeeds on Attach — streams
// real command output just like the no-auth "attach executes input..." test.
test("attach succeeds with the correct token", async () => {
  const tokenDaemon = await startDaemon({ token: AUTH_TOKEN });
  try {
    const authedClient = await tymuxClient(tokenDaemon.addr, AUTH_TOKEN);
    const session = await authedClient.createSession({ name: "ts-auth-attach-integration", command: "" });
    const node = session.windows[0]?.layout?.node;
    assert.equal(node?.case, "pane", "a fresh session's window should be a single-pane leaf");
    if (node?.case !== "pane") throw new Error("unreachable");
    const paneId = node.value.id;

    const { output } = await runAttachDemo(paneId, { baseUrl: tokenDaemon.addr, token: AUTH_TOKEN });
    assert.ok(output.includes("tymux-ts-marker-output"), "attach should observe the command's real output");
  } finally {
    tokenDaemon.stop();
  }
});

// --- Epic 5.1: resume support ---

// A stalled RPC (rather than an explicit error) must still fail the test
// promptly instead of hanging until node:test's own (effectively unbounded)
// default timeout — mirrors the daemon's own resume tests' use of
// `tokio::time::timeout` around every blocking `inbound.message()` call.
function withTimeout<T>(promise: Promise<T>, ms: number, label: string): Promise<T> {
  return Promise.race([
    promise,
    new Promise<T>((_resolve, reject) => setTimeout(() => reject(new Error(`${label} timed out after ${ms}ms`)), ms)),
  ]);
}

type CollectedChunk = { seq: bigint; text: string };

// Low-level attach loop (below runAttachDemo's demo-shaped API) used by the
// resume tests below, which need fine control over when to disconnect and
// exactly which OutputChunk seq/data pairs were observed — mirrors the
// daemon's own resume tests' shape (subscribe/collect, then assert on the
// collected seq sequence), just driven over the real gRPC client instead of
// crates/tymux-core's Pane directly.
async function attachAndCollectChunks(
  paneId: string,
  resumeFromSeq: bigint | undefined,
  inputScript: string | undefined,
  stopAfter: (collected: CollectedChunk[]) => boolean,
): Promise<CollectedChunk[]> {
  const controller = new AbortController();
  const collected: CollectedChunk[] = [];

  async function* requests(): AsyncIterable<AttachRequest> {
    yield { payload: { case: "paneId", value: paneId }, resumeFromSeq } as AttachRequest;
    if (inputScript !== undefined) {
      yield { payload: { case: "input", value: new TextEncoder().encode(inputScript) } } as AttachRequest;
    }
    // Same full-cancellation-is-detach contract as runAttachDemo: keep the
    // send side open until the caller aborts.
    await new Promise((resolve) => controller.signal.addEventListener("abort", resolve));
  }

  try {
    for await (const event of client.attach(requests(), { signal: controller.signal })) {
      if (event.payload.case === "outputChunk") {
        collected.push({ seq: event.payload.value.seq, text: new TextDecoder().decode(event.payload.value.data) });
        if (stopAfter(collected)) controller.abort();
      } else if (event.payload.case === "exited") {
        break;
      }
      // gapExceeded/snapshot/heartbeat priming events are deliberately
      // ignored here — this helper only cares about the seq'd live tail.
    }
  } catch (err) {
    if (!controller.signal.aborted) throw err;
  }

  return collected;
}

// Story 5.1.1 AC2 / Task 5.1.1b: disconnect mid-stream, reattach with the
// last-seen seq, and prove the two streams' OutputChunks concatenate with
// no gap and no duplicate — exactly what one uninterrupted stream would
// have produced. Markers use the same "%s stays literal in the echoed
// input, only the substituted stdout contains the real text" trick as
// runAttachDemo's own marker, so each marker can only appear once in the
// combined stream regardless of pty local-echo of the typed script.
test("attach resumes byte-identically after disconnect and reattach with recorded seq", { timeout: 30_000 }, async () => {
  const session = await client.createSession({ name: "ts-resume-integration", command: "" });
  const node = session.windows[0]?.layout?.node;
  assert.equal(node?.case, "pane", "a fresh session's window should be a single-pane leaf");
  if (node?.case !== "pane") throw new Error("unreachable");
  const paneId = node.value.id;

  const MARKER_COUNT = 8;
  const markers = Array.from({ length: MARKER_COUNT }, (_, i) => `tymux-resume-marker-${i + 1}`);
  const doneMarker = "tymux-resume-complete-done";
  const markerList = Array.from({ length: MARKER_COUNT }, (_, i) => i + 1).join(" ");
  // One single-line script, sent as one input write, so it keeps running
  // on the pty after we disconnect (full-cancellation only detaches, per
  // the test above). Real sleeps between iterations — not just a fast
  // shell-side loop — so consecutive printfs land as separate pty reads
  // instead of coalescing into one big OutputChunk (matches the daemon's
  // own `attach_should_send_exited_event_and_not_hang_when_pane_process_...`
  // test's documented finding).
  const script =
    `for i in ${markerList}; do printf 'tymux-resume-marker-%s\\n' "$i"; sleep 0.05; done; ` +
    `printf 'tymux-resume-complete-%s\\n' done\n`;

  // First attach: Some(0n), the "no resume state yet, but I want seq'd
  // output going forward" sentinel (AttachRequest.resume_from_seq's own
  // doc comment) — send the script, then disconnect partway through,
  // well before the completion marker.
  const firstChunks = await attachAndCollectChunks(paneId, 0n, script, (collected) => collected.length >= 3);
  assert.ok(firstChunks.length >= 3, "should have collected some chunks before disconnecting");
  assert.ok(
    !firstChunks
      .map((c) => c.text)
      .join("")
      .includes(doneMarker),
    "test setup: disconnect should land before the script finishes",
  );
  const lastSeq = firstChunks[firstChunks.length - 1].seq;

  // Second attach: reattach with the recorded seq. No more input to send —
  // the pane's shell is still executing the script we already sent — just
  // wait for the completion marker.
  const secondChunks = await attachAndCollectChunks(paneId, lastSeq, undefined, (collected) =>
    collected
      .map((c) => c.text)
      .join("")
      .includes(doneMarker),
  );

  const combined = [...firstChunks, ...secondChunks];
  for (let i = 1; i < combined.length; i++) {
    assert.equal(
      combined[i].seq,
      combined[i - 1].seq + 1n,
      `OutputChunk seq must be contiguous across the reconnect: ${combined[i - 1].seq} -> ${combined[i].seq}`,
    );
  }

  const combinedText = combined.map((c) => c.text).join("");
  for (const marker of markers) {
    const occurrences = combinedText.split(marker).length - 1;
    assert.equal(occurrences, 1, `${marker} should appear exactly once in the combined stream, got ${occurrences}`);
  }
  assert.ok(combinedText.includes(doneMarker), "combined stream should include the completion marker");
});

// Story 5.1.1 AC3 / Task 5.1.1c: a resume_from_seq older than anything the
// daemon's ReplayBuffer still retains falls back to GapExceeded followed
// immediately by a fresh Snapshot — mirrors the daemon's own
// `attach_should_emit_gap_exceeded_then_snapshot_when_resume_from_seq_is_stale_and_evicted`,
// flooding well past the default 256 KiB per-pane replay budget (no
// smaller test-only budget is exposed over the wire) to force real
// eviction rather than a synthetic one.
test("attach receives gapExceeded then snapshot when resumeFromSeq is stale and evicted", { timeout: 45_000 }, async () => {
  const session = await client.createSession({ name: "ts-gap-exceeded-integration", command: "" });
  const node = session.windows[0]?.layout?.node;
  assert.equal(node?.case, "pane", "a fresh session's window should be a single-pane leaf");
  if (node?.case !== "pane") throw new Error("unreachable");
  const paneId = node.value.id;

  // A pane's very first-ever chunk is always seq == 1 — guaranteed to
  // predate the flood below, so it's a valid stale resume point with no
  // need to settle on a specific marker first.
  const staleResumeFromSeq = 1n;

  // 80,000 lines of ~10 bytes each (~780 KiB) is roughly 3x the daemon's
  // default 256 KiB per-pane replay budget, leaving real margin so eviction
  // of seq 1 doesn't depend on the flood landing exactly at the boundary.
  const LINE_COUNT = 80_000;
  // "FLOOD-D''ONE" (shell string concatenation) keeps the literal
  // "FLOOD-DONE" text out of the echoed keystrokes — same trick as
  // clients/go's equivalent test — so screenShowsFloodDone() below can't
  // false-positive on the pty's local echo of the typed command itself,
  // only on the real printf output.
  const floodCmd = `i=0; while [ $i -lt ${LINE_COUNT} ]; do printf 'L%07dE\\n' "$i"; i=$((i+1)); done; printf 'FLOOD-D''ONE\\n'\n`;

  const floodController = new AbortController();
  async function* floodRequests(): AsyncIterable<AttachRequest> {
    yield { payload: { case: "paneId", value: paneId } } as AttachRequest;
    yield { payload: { case: "input", value: new TextEncoder().encode(floodCmd) } } as AttachRequest;
    await new Promise((resolve) => floodController.signal.addEventListener("abort", resolve));
  }
  const floodDrain = (async () => {
    try {
      for await (const _event of client.attach(floodRequests(), { signal: floodController.signal })) {
        // Drain only — completion is detected independently via
        // capturePane below, since a broadcast subscriber can legitimately
        // drop frames under this much volume (same rationale as the
        // daemon's own equivalent test).
      }
    } catch (err) {
      if (!floodController.signal.aborted) throw err;
    }
  })();

  const deadline = Date.now() + 30_000;
  const screenShowsFloodDone = async (): Promise<boolean> => {
    const snapshot = await withTimeout(capturePane(paneId, daemon.addr), 5_000, "capturePane");
    const screenText = snapshot.grid.map((row) => row.cells.map((cell) => cell.text).join("")).join("\n");
    return screenText.includes("FLOOD-DONE");
  };
  for (;;) {
    if (await screenShowsFloodDone()) break;
    assert.ok(Date.now() < deadline, "flood never completed");
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  // Settle: the rendered grid and the replay buffer are updated by the same
  // reader loop, but re-confirm after a short grace period rather than
  // trusting the single instant "FLOOD-DONE" first appeared — closes any
  // race between the grid becoming visible and the replay buffer's own
  // eviction bookkeeping catching up (mirrors the daemon's own flood test's
  // "settle" pattern for its 150-chunk backlog).
  await new Promise((resolve) => setTimeout(resolve, 300));
  assert.ok(await screenShowsFloodDone(), "flood output should still be settled after the grace period");
  floodController.abort();
  await floodDrain;

  const resumeController = new AbortController();
  async function* resumeRequests(): AsyncIterable<AttachRequest> {
    yield { payload: { case: "paneId", value: paneId }, resumeFromSeq: staleResumeFromSeq } as AttachRequest;
    await new Promise((resolve) => resumeController.signal.addEventListener("abort", resolve));
  }
  const iterator = client.attach(resumeRequests(), { signal: resumeController.signal })[Symbol.asyncIterator]();

  const first = await withTimeout(iterator.next(), 5_000, "attach must respond with the first event");
  assert.ok(!first.done, "stream ended before any event");
  assert.equal(first.value.payload.case, "gapExceeded");
  if (first.value.payload.case === "gapExceeded") {
    assert.ok(
      first.value.payload.value.oldestAvailableSeq > staleResumeFromSeq,
      "oldestAvailableSeq must be past the now-stale resume point",
    );
  }

  const second = await withTimeout(iterator.next(), 5_000, "attach must respond with the fallback Snapshot event");
  assert.ok(!second.done, "stream ended before the fallback Snapshot event");
  assert.equal(second.value.payload.case, "snapshot");

  resumeController.abort();
  try {
    await iterator.next();
  } catch {
    // Expected: the abort cancels the underlying stream.
  }
});

// --- Epic 8.2 Story 8.2.2 / Task 8.2.2b: tymuxClient()'s UDS-first dial
// order, logged TCP fallback, and EACCES hard-error handling. These stand
// up their own minimal connect-node servers (a real UDS-bound h2c server /
// a real TCP-bound h2c server) rather than using startDaemon() -- what's
// under test here is tymuxClient()'s own dial-order logic in
// examples/client.ts, not tymuxd's UDS support (crates/tymuxd's Epic 2.2
// UDS-listener wiring is landing concurrently on this branch and this
// phase's TS work does not depend on it).

function stubTymuxServer(): (router: ConnectRouter) => void {
  return (router) => {
    router.service(TymuxService, { listSessions: () => ({ sessions: [] }) });
  };
}

async function withSocketPathEnv<T>(value: string, fn: () => Promise<T>): Promise<T> {
  const saved = process.env.TYMUXD_SOCKET_PATH;
  process.env.TYMUXD_SOCKET_PATH = value;
  try {
    return await fn();
  } finally {
    if (saved === undefined) delete process.env.TYMUXD_SOCKET_PATH;
    else process.env.TYMUXD_SOCKET_PATH = saved;
  }
}

test("tymuxClient() dials the resolved Unix socket first when it's reachable", async () => {
  const stateDir = mkdtempSync(join(tmpdir(), "tymux-ts-uds-first-"));
  const socketPath = join(stateDir, "tymuxd.sock");
  const udsServer = http2.createServer(connectNodeAdapter({ routes: stubTymuxServer() }));
  await new Promise<void>((resolve) => udsServer.listen(socketPath, resolve));

  const originalError = console.error;
  const errors: unknown[] = [];
  console.error = (...args: unknown[]) => errors.push(args);
  try {
    await withSocketPathEnv(socketPath, async () => {
      const udsClient = await tymuxClient();
      const response = await udsClient.listSessions({});
      assert.deepEqual(response.sessions, []);
    });
  } finally {
    console.error = originalError;
    await new Promise<void>((resolve) => udsServer.close(() => resolve()));
    rmSync(stateDir, { recursive: true, force: true });
  }
  assert.equal(errors.length, 0, "no fallback notice should be printed when UDS is reachable");
});

// The TCP fallback target is a fixed address (DEFAULT_TCP_FALLBACK_URL,
// matching tymux-cli's/clients/go's default), so this test binds that
// exact port itself rather than spawning startDaemon() (which uses
// ephemeral ports for isolation). Skips gracefully if something else on
// the machine already holds that port -- e.g. a real tymuxd -- rather than
// failing on an environmental conflict outside this test's control.
test("tymuxClient() falls back to TCP loopback with exactly one notice when the Unix socket is unreachable", async (t) => {
  const stateDir = mkdtempSync(join(tmpdir(), "tymux-ts-uds-fallback-"));
  const missingSocketPath = join(stateDir, "does-not-exist.sock");
  const tcpServer = http2.createServer(connectNodeAdapter({ routes: stubTymuxServer() }));

  try {
    await new Promise<void>((resolve, reject) => {
      tcpServer.once("error", reject);
      tcpServer.listen(7419, "127.0.0.1", resolve);
    });
  } catch (err) {
    rmSync(stateDir, { recursive: true, force: true });
    if ((err as NodeJS.ErrnoException).code === "EADDRINUSE") {
      t.skip("port 7419 already in use on this machine -- cannot exercise the fixed TCP fallback address");
      return;
    }
    throw err;
  }

  const originalError = console.error;
  const errors: string[] = [];
  console.error = (...args: unknown[]) => errors.push(args.join(" "));
  try {
    await withSocketPathEnv(missingSocketPath, async () => {
      const fallbackClient = await tymuxClient();
      const response = await fallbackClient.listSessions({});
      assert.deepEqual(response.sessions, []);
    });
  } finally {
    console.error = originalError;
    await new Promise<void>((resolve) => tcpServer.close(() => resolve()));
    rmSync(stateDir, { recursive: true, force: true });
  }
  assert.equal(errors.length, 1, "exactly one fallback notice should be printed");
  assert.match(errors[0], /falling back to TCP loopback/);
});

// pre-mortem.md P1 #1: a daemon IS listening at socketPath and the kernel
// denied the connect() itself -- this must never fall back to the
// unauthenticated TCP path. No TCP listener is started in this test, so a
// wrongly-attempted fallback would surface as ECONNREFUSED instead of this
// asserted message.
test("tymuxClient() rejects with a hard error on EACCES and never falls back to TCP", async () => {
  const stateDir = mkdtempSync(join(tmpdir(), "tymux-ts-uds-eacces-"));
  const socketPath = join(stateDir, "tymuxd.sock");
  const guardServer = net.createServer(() => {});
  await new Promise<void>((resolve) => guardServer.listen(socketPath, resolve));
  chmodSync(socketPath, 0o000);

  try {
    await withSocketPathEnv(socketPath, async () => {
      await assert.rejects(
        () => tymuxClient(),
        (err: unknown) => {
          assert.ok(err instanceof Error);
          assert.match(err.message, /not authorized to access this daemon's socket/);
          return true;
        },
      );
    });
  } finally {
    guardServer.close();
    rmSync(stateDir, { recursive: true, force: true });
  }
});

// --- Epic 8.3 Story 8.3.1: real tymuxd UDS accept/reject. tymuxd didn't
// bind a real UDS listener when Epic 8.2's tests above were written, so
// those stand up self-hosted stub servers instead; now that tymuxd binds a
// real dual TCP+UDS listener (commit b44aae1) with peer-cred auth, these
// dial an actual spawned daemon via tymuxClient() itself.

// Task 8.3.1b: same-uid accept -- the only uid available to a Node test
// process without root (pitfalls.md §7) -- proves the full UDS-first dial
// path round-trips real RPCs against a real daemon, not just a stub.
test("tymuxClient() dials a real tymuxd over UDS and round-trips a session (same-uid accept)", async () => {
  const stateDir = mkdtempSync(join(tmpdir(), "tymux-ts-uds-accept-"));
  const socketPath = join(stateDir, "tymuxd.sock");
  const udsDaemon = await startDaemon({ socketPath });
  try {
    await withSocketPathEnv(socketPath, async () => {
      const udsClient = await tymuxClient();
      const created = await udsClient.createSession({ name: "ts-uds-accept-integration", command: "" });
      const listed = await udsClient.listSessions({});
      const found = listed.sessions.find((s) => s.id === created.id);
      assert.ok(found, "a session created over the real UDS listener should appear in listSessions over the same socket");
    });
  } finally {
    udsDaemon.stop();
    rmSync(stateDir, { recursive: true, force: true });
  }
});

// Task 8.3.1c: the true cross-uid reject proof. Requires root/CAP_SETUID to
// spawn a child process under a genuinely different real OS uid
// (`child_process.spawn`'s `uid` option) -- ships skipped by default on
// this repo's actual CI (plain ubuntu-latest/macos-latest runners, no
// root/CAP_SETUID; confirmed against .github/workflows/ci.yml at planning
// time), mirroring Go's Task 7.3.1b and tymux-cli's Task 6.4.1c. See
// plan.md's "Unresolved Questions" (resolved during planning) and
// pitfalls.md §7. `peer_is_authorized`'s own unit tests
// (crates/tymuxd/src/auth.rs) are the accepted substitute proof for the
// authorization *decision*; this test only proves the real kernel-level
// gate when it can actually run as root.
test(
  "a client connecting from a genuinely different OS uid is rejected",
  { skip: process.getuid!() !== 0 ? "requires root/CAP_SETUID to spawn a child under a different uid" : false },
  async () => {
    const stateDir = mkdtempSync(join(tmpdir(), "tymux-ts-uds-cross-uid-"));
    const socketPath = join(stateDir, "tymuxd.sock");
    const udsDaemon = await startDaemon({ socketPath });
    try {
      // "nobody" -- present on essentially every Linux/macOS install, and
      // guaranteed distinct from uid 0 (the daemon's own uid, since this
      // test only runs at all when the test process itself is root).
      const DIFFERENT_UID = 65534;

      // A minimal standalone probe, not the full tymuxClient() stack:
      // auth::bind_uds_listener binds tymuxd's UDS socket file mode 0600
      // (owner-only) by default, so the OS itself denies connect() to any
      // non-owning uid before the daemon's own peer_is_authorized ever
      // runs -- the same EACCES branch examples/client.ts's probeUdsSocket
      // already classifies as "not authorized" (see the EACCES test
      // above). Spawning this probe under a genuinely different uid (via
      // spawn's `uid` option) is what actually requires root, unlike that
      // chmod-based test; a plain CJS `-e` script avoids needing tsx/ESM
      // module resolution to succeed under the restricted child uid.
      const probe = [
        'const net = require("node:net");',
        `const socket = net.connect({ path: ${JSON.stringify(socketPath)} });`,
        'socket.once("connect", () => { console.log("CONNECTED"); socket.destroy(); process.exit(0); });',
        'socket.once("error", (err) => { console.log("ERROR:" + err.code); process.exit(1); });',
      ].join("\n");

      const child = spawn(process.execPath, ["-e", probe], {
        uid: DIFFERENT_UID,
        stdio: ["ignore", "pipe", "pipe"],
      });
      let stdout = "";
      child.stdout.on("data", (chunk: Buffer) => {
        stdout += chunk.toString();
      });
      const exitCode = await new Promise<number | null>((resolve) => child.on("exit", resolve));

      assert.notEqual(exitCode, 0, "a genuinely different uid must not be able to connect to the daemon's UDS socket");
      assert.match(stdout, /ERROR:EACCES/, "the kernel must deny connect() with EACCES for a non-owning uid");
    } finally {
      udsDaemon.stop();
      rmSync(stateDir, { recursive: true, force: true });
    }
  },
);
