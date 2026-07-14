import { tymuxClient } from "./client.js";

const client = tymuxClient();
const response = await client.listSessions({});
for (const session of response.sessions) {
  console.log(`${session.id}\t${session.name}\t${session.liveness}`);
}
