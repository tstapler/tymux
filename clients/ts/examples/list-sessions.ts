import { tymuxClient } from "./client.js";
import type { Layout } from "../gen/tymux/v1/tymux_pb.js";

// Every leaf pane_id in a window's layout tree, in tree order — the
// README points here for pane IDs to pass to `attach`/`capture-pane`,
// so this must actually print them (previously only session-level
// fields were printed, leaving no documented way to get a pane_id from
// this script's own output).
function flattenPaneIds(layout: Layout | undefined): string[] {
  if (!layout?.node) return [];
  if (layout.node.case === "pane") return [layout.node.value.id];
  if (layout.node.case === "split") {
    return layout.node.value.children.flatMap((child) => flattenPaneIds(child.layout));
  }
  return [];
}

const client = tymuxClient();
const response = await client.listSessions({});
for (const session of response.sessions) {
  console.log(`${session.id}\t${session.name}\t${session.liveness}`);
  for (const window of session.windows) {
    for (const paneId of flattenPaneIds(window.layout)) {
      console.log(`  window ${window.name} (${window.id})\tpane ${paneId}`);
    }
  }
}
