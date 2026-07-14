import { tymuxClient } from "./client.js";

export async function capturePane(paneId: string, baseUrl?: string) {
  const client = tymuxClient(baseUrl);
  return client.capturePane({ paneId });
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const paneId = process.argv[2];
  if (!paneId) {
    console.error("usage: capture-pane.ts <pane_id>");
    process.exit(1);
  }
  const snapshot = await capturePane(paneId);
  for (const row of snapshot.grid) {
    console.log(row.cells.map((c) => c.text).join(""));
  }
}
