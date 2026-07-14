import { after, before, test } from "node:test";
import assert from "node:assert/strict";
import { createClient } from "@connectrpc/connect";
import { createGrpcTransport } from "@connectrpc/connect-node";
import { TymuxService } from "../gen/tymux/v1/tymux_pb.js";
import { runAttachDemo } from "../examples/attach.js";
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

  const output = await runAttachDemo(paneId, daemon.addr);
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
