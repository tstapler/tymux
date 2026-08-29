import { tymuxClient } from "./client.js";
import type { AttachRequest } from "../gen/tymux/v1/tymux_pb.js";

export interface AttachDemoOptions {
  baseUrl?: string;
  /**
   * Epic 1.1 / ADR-004 (revised): the highest OutputChunk.seq this caller
   * already has for this pane, threaded onto the first AttachRequest so
   * the daemon replays from its ReplayBuffer starting at seq+1 instead of
   * sending a fresh PaneSnapshot. Omit (undefined) for a full attach with
   * no resume state — identical to a pre-feature client. Some(0n) is the
   * "no resume state yet, but I want seq'd output going forward" sentinel
   * (Story 5.1.1 AC1).
   */
  resumeFromSeq?: bigint;
  /**
   * Bearer token to attach to the underlying transport (Epic 3.2), threaded
   * into `tymuxClient(baseUrl, token)`. Omit for a loopback/no-auth daemon —
   * identical to a pre-feature client.
   */
  token?: string;
}

export interface AttachDemoResult {
  output: string;
  /**
   * The highest OutputChunk.seq observed on this call, if any `output_chunk`
   * events were received (only populated when `resumeFromSeq` was set on
   * the request — see AttachEvent.payload's doc comment). Reuse as
   * `resumeFromSeq` on a subsequent `runAttachDemo` call to continue from
   * here after a disconnect.
   */
  lastSeq?: bigint;
}

// Proves ADR-003's cross-language claim for the two riskiest RPCs:
// Attach's bidi stream, and full-cancellation-is-detach (Epic 2 Story 2.3).
// Story 5.1.1: also proves the resume path cross-language — passing
// `resumeFromSeq` switches the live tail from the plain `output` field to
// the seq'd `output_chunk` field, letting a caller track how far it got.
export async function runAttachDemo(paneId: string, options: AttachDemoOptions = {}): Promise<AttachDemoResult> {
  const { baseUrl, resumeFromSeq, token } = options;
  const client = tymuxClient(baseUrl, token);
  const controller = new AbortController();

  async function* requests(): AsyncIterable<AttachRequest> {
    yield { payload: { case: "paneId", value: paneId }, resumeFromSeq } as AttachRequest;
    yield {
      payload: { case: "input", value: new TextEncoder().encode("printf 'tymux-ts-marker-%s\\n' output\n") },
    } as AttachRequest;
    // Keep the generator alive until the caller aborts — closing this send
    // side alone does not end the attach (see the RPC's doc comment); only
    // full cancellation via controller.abort() does.
    await new Promise((resolve) => controller.signal.addEventListener("abort", resolve));
  }

  const chunks: string[] = [];
  let lastSeq: bigint | undefined;
  try {
    for await (const event of client.attach(requests(), { signal: controller.signal })) {
      if (event.payload.case === "outputChunk") {
        const text = new TextDecoder().decode(event.payload.value.data);
        lastSeq = event.payload.value.seq;
        chunks.push(text);
        // The typed command echoes back as raw keystrokes containing the
        // literal "%s" placeholder, not the substituted value — so this
        // exact string can only appear once printf has actually run.
        if (chunks.join("").includes("tymux-ts-marker-output")) {
          controller.abort();
        }
      } else if (event.payload.case === "output") {
        // Pre-resume / no-resume-token path: `output` and `output_chunk`
        // are mutually exclusive siblings of the same oneof (see
        // AttachEvent.payload's doc comment) — this is the one the
        // daemon populates when `resumeFromSeq` was omitted.
        const text = new TextDecoder().decode(event.payload.value);
        chunks.push(text);
        if (chunks.join("").includes("tymux-ts-marker-output")) {
          controller.abort();
        }
      } else if (event.payload.case === "exited") {
        break;
      }
    }
  } catch (err) {
    // AbortController-triggered cancellation surfaces as a "Cancelled" connect error — expected.
    if (!controller.signal.aborted) throw err;
  }

  return { output: chunks.join(""), lastSeq };
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const paneId = process.argv[2];
  if (!paneId) {
    console.error("usage: attach.ts <pane_id> [resume_from_seq]");
    process.exit(1);
  }
  const resumeArg = process.argv[3];
  const { output } = await runAttachDemo(paneId, resumeArg !== undefined ? { resumeFromSeq: BigInt(resumeArg) } : {});
  console.log(output);
}
