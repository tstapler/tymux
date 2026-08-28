import { createClient, type Interceptor } from "@connectrpc/connect";
import { createGrpcTransport } from "@connectrpc/connect-node";
import { TymuxService } from "../gen/tymux/v1/tymux_pb.js";

// Attaches the configured bearer token to every outgoing call. Applies
// uniformly to unary and streaming calls by construction — TS has one
// Interceptor type, not Go's separate unary/streaming split. A no-op when
// no token is configured, matching every other client stack's "empty is
// absent" treatment.
function authInterceptor(token?: string): Interceptor {
  return (next) => async (req) => {
    if (token) req.header.set("authorization", `Bearer ${token}`);
    return await next(req);
  };
}

// Shared transport factory for every example script. tymuxd requires no
// auth on loopback binds; a non-loopback tymuxd requires a bearer token,
// attached here via authInterceptor when one is provided.
export function tymuxClient(baseUrl = "http://127.0.0.1:7419", token?: string) {
  const transport = createGrpcTransport({ baseUrl, interceptors: [authInterceptor(token)] });
  return createClient(TymuxService, transport);
}
