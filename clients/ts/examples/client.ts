import { createClient } from "@connectrpc/connect";
import { createGrpcTransport } from "@connectrpc/connect-node";
import { TymuxService } from "../gen/tymux/v1/tymux_pb.js";

// Shared transport factory for every example script — tymuxd listens on
// loopback only (ADR: loopback-trust security model), so there is no TLS
// setup here.
export function tymuxClient(baseUrl = "http://127.0.0.1:7419") {
  const transport = createGrpcTransport({ baseUrl });
  return createClient(TymuxService, transport);
}
