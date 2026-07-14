import { tymuxClient } from "./client.js";
import type { AttachRequest } from "../gen/tymux/v1/tymux_pb.js";

// Proves ADR-003's cross-language claim for the two riskiest RPCs:
// Attach's bidi stream, and full-cancellation-is-detach (Epic 2 Story 2.3).
export async function runAttachDemo(paneId: string, baseUrl?: string) {
  const client = tymuxClient(baseUrl);
  const controller = new AbortController();

  async function* requests(): AsyncIterable<AttachRequest> {
    yield { payload: { case: "paneId", value: paneId } } as AttachRequest;
    yield {
      payload: { case: "input", value: new TextEncoder().encode("printf 'tymux-ts-marker-%s\\n' output\n") },
    } as AttachRequest;
    // Keep the generator alive until the caller aborts — closing this send
    // side alone does not end the attach (see the RPC's doc comment); only
    // full cancellation via controller.abort() does.
    await new Promise((resolve) => controller.signal.addEventListener("abort", resolve));
  }

  const chunks: string[] = [];
  try {
    for await (const event of client.attach(requests(), { signal: controller.signal })) {
      if (event.payload.case === "output") {
        const text = new TextDecoder().decode(event.payload.value);
        chunks.push(text);
        // The typed command echoes back as raw keystrokes containing the
        // literal "%s" placeholder, not the substituted value — so this
        // exact string can only appear once printf has actually run.
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

  return chunks.join("");
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const paneId = process.argv[2];
  if (!paneId) {
    console.error("usage: attach.ts <pane_id>");
    process.exit(1);
  }
  const output = await runAttachDemo(paneId);
  console.log(output);
}
