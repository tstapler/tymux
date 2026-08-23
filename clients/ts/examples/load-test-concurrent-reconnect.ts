import { tymuxClient } from "./client.js";
import type { AttachRequest, Layout } from "../gen/tymux/v1/tymux_pb.js";

// Epic 1.7 Story 1.7.3 / pre-mortem P1 #2: Story 1.7.2 only measured a
// concurrent CreateSession burst, not requirements.md's own named danger
// scenario — a mass RECONNECT of ~1,000 standing Attach streams (e.g. after
// a stapler-squad restart or a tymuxd upgrade). That path (per-pane
// broadcast resubscribe + priming-snapshot resync, Epic 1.3) was only ever
// exercised for a single stream. This harness drives it at concurrency.
//
// BackendTymux's own ReconnectLoop (stapler-squad-side) doesn't exist yet
// (Epic 2.5) — so this measures what's actually testable right now: N TS
// clients each hold a standing Attach stream, all N are dropped
// simultaneously (client-side abort, no graceful detach), then all N
// re-open a fresh Attach to the same pane_id concurrently. That is
// tymuxd's own server-side reconnect-capable behavior under burst load,
// the piece Epic 1.7 can validate today; a full stapler-squad-side
// ReconnectLoop load test happens later in Epic 2.5.

const N = Number(process.env.LOAD_TEST_RECONNECT_N ?? 1000);

// Mirrors list-sessions.ts's leaf-pane-id walk (not exported there since
// that script runs immediately on import; duplicated here rather than
// refactoring list-sessions.ts's shape for one shared helper).
function flattenPaneIds(layout: Layout | undefined): string[] {
  if (!layout?.node) return [];
  if (layout.node.case === "pane") return [layout.node.value.id];
  if (layout.node.case === "split") {
    return layout.node.value.children.flatMap((child) => flattenPaneIds(child.layout));
  }
  return [];
}

function percentile(sorted: number[], p: number): number {
  if (sorted.length === 0) return NaN;
  const idx = Math.min(sorted.length - 1, Math.ceil((p / 100) * sorted.length) - 1);
  return sorted[Math.max(0, idx)];
}

interface AttachHandle {
  controller: AbortController;
  // Resolves with elapsed ms from open to first Snapshot event; rejects if
  // the stream errors or is aborted before a snapshot ever arrives.
  ready: Promise<number>;
  // Resolves once the consuming loop exits (aborted, exited, or errored).
  done: Promise<void>;
}

function openAttach(client: ReturnType<typeof tymuxClient>, paneId: string): AttachHandle {
  const controller = new AbortController();
  const start = performance.now();
  let resolveReady!: (elapsedMs: number) => void;
  let rejectReady!: (err: unknown) => void;
  const ready = new Promise<number>((resolve, reject) => {
    resolveReady = resolve;
    rejectReady = reject;
  });

  async function* requests(): AsyncIterable<AttachRequest> {
    yield { payload: { case: "paneId", value: paneId } } as AttachRequest;
    // Keep the send side alive until the caller deliberately aborts — same
    // pattern as attach.ts; only full cancellation ends the attach.
    await new Promise((resolve) => controller.signal.addEventListener("abort", resolve));
  }

  const done = (async () => {
    let gotSnapshot = false;
    try {
      for await (const event of client.attach(requests(), { signal: controller.signal })) {
        if (event.payload.case === "snapshot" && !gotSnapshot) {
          gotSnapshot = true;
          resolveReady(performance.now() - start);
        } else if (event.payload.case === "exited") {
          break;
        }
      }
      if (!gotSnapshot) rejectReady(new Error("attach stream ended before any snapshot"));
    } catch (err) {
      if (!gotSnapshot) rejectReady(err);
    }
  })();

  return { controller, ready, done };
}

async function main() {
  const client = tymuxClient();

  // Empty command -> daemon default ($SHELL): an idle shell, matching
  // scale-feasibility.md's methodology, so the pane stays alive to attach to.
  console.log(`pre-creating ${N} sessions sequentially...`);
  const preStart = Date.now();
  const paneIds: string[] = [];
  for (let i = 0; i < N; i++) {
    const session = await client.createSession({ name: `reconnect-${i}` });
    const ids = session.windows.flatMap((w) => flattenPaneIds(w.layout));
    if (ids.length === 0) throw new Error(`session ${session.id} has no pane`);
    paneIds.push(ids[0]);
  }
  console.log(`pre-creation done in ${Date.now() - preStart}ms`);

  console.log(`opening ${N} standing Attach streams...`);
  const initialOpenStart = Date.now();
  const standing = paneIds.map((id) => openAttach(client, id));
  const initialResults = await Promise.allSettled(standing.map((h) => h.ready));
  const initialFailures = initialResults.filter((r) => r.status === "rejected").length;
  console.log(
    `standing streams established in ${Date.now() - initialOpenStart}ms ` +
      `(${N - initialFailures}/${N} confirmed via first snapshot, ${initialFailures} failed to establish)`,
  );
  if (initialFailures > 0) {
    console.warn("some standing streams failed to establish — reconnect measurement below only covers panes with a live pane_id regardless");
  }

  // Drop all N standing streams simultaneously — client-side abort with no
  // graceful detach, simulating a stapler-squad-side restart from tymuxd's
  // perspective — then immediately re-open a fresh Attach to every pane_id
  // concurrently, timing send -> first Snapshot event per reconnect.
  console.log(`dropping all ${N} streams simultaneously and reconnecting...`);
  const dropStart = Date.now();
  standing.forEach((h) => h.controller.abort());

  const reconnectStart = performance.now();
  const reconnects = paneIds.map((id) => openAttach(client, id));
  const reconnectResults = await Promise.allSettled(reconnects.map((h) => h.ready));
  const reconnectWallMs = performance.now() - reconnectStart;
  console.log(`drop-to-reconnect-complete wall time: ${reconnectWallMs.toFixed(2)}ms (drop issued at +${Date.now() - dropStart}ms)`);

  const latencies: number[] = [];
  let failures = 0;
  for (const r of reconnectResults) {
    if (r.status === "fulfilled") latencies.push(r.value);
    else failures++;
  }
  latencies.sort((a, b) => a - b);
  const p50 = percentile(latencies, 50);
  const p99 = percentile(latencies, 99);
  const max = latencies.length ? latencies[latencies.length - 1] : NaN;

  console.log("--- results ---");
  console.log(`n=${N} reconnect_failures=${failures} reconnect_successes=${latencies.length}`);
  console.log(`time-to-first-snapshot: p50=${p50.toFixed(2)}ms p99=${p99.toFixed(2)}ms max=${max.toFixed(2)}ms`);
  console.log(
    `threshold check: p99 < 2000ms -> ${p99 < 2000 ? "PASS" : "FAIL"} (p99=${p99.toFixed(2)}ms); ` +
      `zero hard failures -> ${failures === 0 ? "PASS" : "FAIL"} (failures=${failures})`,
  );

  // Clean up remaining open streams so the process can exit.
  reconnects.forEach((h) => h.controller.abort());
  await Promise.allSettled(reconnects.map((h) => h.done));
  await Promise.allSettled(standing.map((h) => h.done));
}

main()
  .then(() => process.exit(0))
  .catch((err) => {
    console.error(err);
    process.exit(1);
  });
