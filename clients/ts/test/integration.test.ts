import { after, before, test } from "node:test";
import assert from "node:assert/strict";
import { Code, ConnectError, createClient } from "@connectrpc/connect";
import { createGrpcTransport } from "@connectrpc/connect-node";
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
    const unauthedClient = tymuxClient(tokenDaemon.addr);
    await assert.rejects(() => unauthedClient.listSessions({}), assertUnauthenticated);
  } finally {
    tokenDaemon.stop();
  }
});

// Story 3.2.1 AC2 / Task 3.2.1c: correct token succeeds on a unary call.
test("listSessions succeeds with the correct token", async () => {
  const tokenDaemon = await startDaemon({ token: AUTH_TOKEN });
  try {
    const authedClient = tymuxClient(tokenDaemon.addr, AUTH_TOKEN);
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
    const authedClient = tymuxClient(tokenDaemon.addr, AUTH_TOKEN);
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
