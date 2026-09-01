import * as net from "node:net";
import * as path from "node:path";
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

function buildTcpClient(baseUrl: string, token?: string) {
  const transport = createGrpcTransport({ baseUrl, interceptors: [authInterceptor(token)] });
  return createClient(TymuxService, transport);
}

// Epic 8.1: dials tymuxd over a Unix domain socket. connect-node's
// createGrpcTransport threads `nodeOptions` straight into Node's
// `http2.connect()`, which itself accepts a `createConnection` callback
// (https://nodejs.org/api/http2.html#http2connectauthority-options-listener)
// — returning a UDS-connected net.Socket here makes http2 dial the socket
// instead of opening a TCP connection to `baseUrl`'s authority. Confirmed
// against a real UDS-bound connect-node server (Epic 8.1's spike) in
// test/uds-transport.test.ts — see that file's header comment for what was
// verified. `baseUrl` is a placeholder: createConnection's return value is
// used as-is, so its host/port are never actually dialed.
export function createUdsGrpcTransport(socketPath: string, token?: string) {
  const transport = createGrpcTransport({
    baseUrl: "http://localhost",
    nodeOptions: { createConnection: () => net.connect({ path: socketPath }) },
    interceptors: [authInterceptor(token)],
  });
  return createClient(TymuxService, transport);
}

// Epic 8.2: mirrors tymuxd's auth::default_uds_socket_path byte-for-byte —
// see plan.md Pattern Decisions row 10. Any change here must be mirrored in
// all four implementations (tymuxd, tymux-cli, clients/go, clients/ts).
export function defaultSocketPath(uid: number): string {
  const xdg = process.env.XDG_RUNTIME_DIR;
  if (xdg) return path.join(xdg, "tymuxd", "tymuxd.sock");
  const base = process.env.TMPDIR || "/tmp";
  return path.join(base, `tymuxd-${uid}`, "tymuxd.sock");
}

// Mirrors tymuxd's auth::resolve_uds_socket_path: TYMUXD_SOCKET_PATH (empty
// treated as unset) wins over the computed default.
export function resolveSocketPath(uid: number): string {
  return process.env.TYMUXD_SOCKET_PATH || defaultSocketPath(uid);
}

const DEFAULT_TCP_FALLBACK_URL = "http://127.0.0.1:7419";

// Eagerly probes whether socketPath is dialable, without holding the probe
// connection open — the real gRPC session is established separately by
// createUdsGrpcTransport's own per-session createConnection callback. This
// is what lets tymuxClient() decide UDS-vs-TCP synchronously up front
// rather than discovering the failure lazily on the first RPC.
function probeUdsSocket(socketPath: string): Promise<void> {
  return new Promise((resolve, reject) => {
    const socket = net.connect({ path: socketPath });
    socket.once("connect", () => {
      socket.destroy();
      resolve();
    });
    socket.once("error", (err) => {
      socket.destroy();
      reject(err);
    });
  });
}

// Shared client factory for every example script. Epic 8.2: with no
// explicit `baseUrl` override, tries the resolved Unix socket first and
// falls back to TCP loopback (with a one-line console notice) when it's
// unreachable — matching tymux-cli's/clients/go's UDS-first dial order. An
// OS-level EACCES on the UDS dial (a daemon IS listening, but we're not
// authorized) is a hard error and never falls back to the unauthenticated
// TCP path (pre-mortem.md P1 #1). Passing an explicit `baseUrl` bypasses
// UDS entirely and dials that address directly over TCP — unchanged from
// this function's pre-Epic-8.2 behavior, so every existing call site that
// pins a specific (e.g. test) daemon address keeps working as before.
export async function tymuxClient(baseUrl?: string, token?: string) {
  if (baseUrl) {
    return buildTcpClient(baseUrl, token);
  }

  const socketPath = resolveSocketPath(process.getuid!());
  try {
    await probeUdsSocket(socketPath);
  } catch (err) {
    const code = (err as NodeJS.ErrnoException)?.code;
    if (code === "EACCES") {
      // A daemon IS listening at socketPath and the kernel denied us the
      // connect() itself -- never fall back to the unauthenticated TCP
      // path for this case (pre-mortem.md P1 #1).
      throw new Error(
        "tymuxd rejected this connection: not authorized to access this daemon's socket " +
          "(ask the daemon's owner to add you to its configured --socket-group, or run " +
          "this client as the daemon's own OS user)",
      );
    }
    // ENOENT ("no socket file") / ECONNREFUSED ("file present, nothing
    // listening") and anything else: no daemon here, legitimate to fall
    // back.
    console.error(
      `no reachable Unix socket at ${socketPath} — falling back to TCP loopback ` +
        `(deprecated; make sure tymuxd is running)`,
    );
    return buildTcpClient(DEFAULT_TCP_FALLBACK_URL, token);
  }
  return createUdsGrpcTransport(socketPath, token);
}
