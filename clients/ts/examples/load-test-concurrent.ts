import { readFileSync, readdirSync } from "node:fs";
import { tymuxClient } from "./client.js";

// Epic 1.7 Story 1.7.1/1.7.2: converts scale-feasibility.md's *inferred*
// concurrent-contention conclusion (measured only strictly sequential
// CreateSession calls) into a measured one, validating Epic 1.4's fix
// (O(n) list_sessions() scan -> O(1) session_snapshot() lookup in
// create_session's handler) under actual concurrent load, not just serial
// load. See project_plans/stapler-squad-integration/research/scale-feasibility.md
// §4/§5 for the methodology this extends.

const PRE_EXISTING = Number(process.env.LOAD_TEST_PRE_EXISTING ?? 900);
const BURST = Number(process.env.LOAD_TEST_BURST ?? 200);
const TYMUXD_PID = process.env.TYMUXD_PID ? Number(process.env.TYMUXD_PID) : undefined;

function sampleProc(pid: number) {
  const status = readFileSync(`/proc/${pid}/status`, "utf8");
  const threads = Number(/^Threads:\s+(\d+)/m.exec(status)?.[1] ?? "NaN");
  const rssKb = Number(/^VmRSS:\s+(\d+) kB/m.exec(status)?.[1] ?? "NaN");
  const fds = readdirSync(`/proc/${pid}/fd`).length;
  return { threads, rssKb, fds };
}

function percentile(sorted: number[], p: number): number {
  if (sorted.length === 0) return NaN;
  const idx = Math.min(sorted.length - 1, Math.ceil((p / 100) * sorted.length) - 1);
  return sorted[Math.max(0, idx)];
}

async function main() {
  const client = await tymuxClient();

  // Empty command -> daemon default ($SHELL), matching scale-feasibility.md's
  // methodology: an idle shell keeps the pane's reader thread alive, so the
  // before/after thread/fd counts actually reflect the measured-linear
  // formula (25 + 1×panes threads, 10 + 3×panes fds) rather than reflecting
  // panes whose process (and reader thread) already exited.
  console.log(`pre-creating ${PRE_EXISTING} sessions sequentially...`);
  const preStart = Date.now();
  for (let i = 0; i < PRE_EXISTING; i++) {
    await client.createSession({ name: `load-pre-${i}` });
  }
  console.log(`pre-creation done in ${Date.now() - preStart}ms`);

  const before = TYMUXD_PID ? sampleProc(TYMUXD_PID) : undefined;
  if (before) console.log("before burst:", before);

  console.log(`firing ${BURST} concurrent CreateSession calls at n≈${PRE_EXISTING}...`);
  const latencies: number[] = [];
  let errors = 0;
  const calls = Array.from({ length: BURST }, async (_, i) => {
    const start = performance.now();
    try {
      await client.createSession({ name: `load-burst-${i}` });
      latencies.push(performance.now() - start);
    } catch (err) {
      errors++;
      console.error(`call ${i} errored:`, err);
    }
  });
  const burstStart = Date.now();
  await Promise.all(calls);
  const burstWallMs = Date.now() - burstStart;

  const after = TYMUXD_PID ? sampleProc(TYMUXD_PID) : undefined;
  if (after) console.log("after burst:", after);

  latencies.sort((a, b) => a - b);
  const p50 = percentile(latencies, 50);
  const p99 = percentile(latencies, 99);
  const max = latencies.length ? latencies[latencies.length - 1] : NaN;

  console.log("--- results ---");
  console.log(`n_pre_existing=${PRE_EXISTING} burst=${BURST} errors=${errors}`);
  console.log(`burst wall time: ${burstWallMs}ms`);
  console.log(`p50=${p50.toFixed(2)}ms p99=${p99.toFixed(2)}ms max=${max.toFixed(2)}ms`);
  if (before && after) {
    console.log(
      `threads: ${before.threads} -> ${after.threads} (expect +${BURST}), ` +
        `fds: ${before.fds} -> ${after.fds} (expect +${3 * BURST}), ` +
        `rss: ${before.rssKb}kB -> ${after.rssKb}kB`,
    );
  }

  const totalSessions = PRE_EXISTING + BURST;
  console.log(
    `threshold check: p99 < 200ms -> ${p99 < 200 ? "PASS" : "FAIL"} (p99=${p99.toFixed(2)}ms); ` +
      `zero errors -> ${errors === 0 ? "PASS" : "FAIL"} (errors=${errors})`,
  );
  console.log(`total sessions after run: ${totalSessions}`);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
