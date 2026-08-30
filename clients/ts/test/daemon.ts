import { spawn, type ChildProcessByStdio } from "node:child_process";
import type { Readable } from "node:stream";
import { accessSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const REPO_ROOT = fileURLToPath(new URL("../../../", import.meta.url));

function resolveBinary(): string {
  if (process.env.TYMUXD_BIN) return process.env.TYMUXD_BIN;
  for (const profile of ["debug", "release"]) {
    const candidate = join(REPO_ROOT, "target", profile, "tymuxd");
    try {
      accessSync(candidate);
      return candidate;
    } catch {
      // try next profile
    }
  }
  throw new Error("tymuxd binary not found — build it first (cargo build --bin tymuxd) or set TYMUXD_BIN");
}

export interface TestDaemon {
  addr: string;
  stop(): void;
}

export interface StartDaemonOptions {
  // When set, binds tymuxd to 0.0.0.0 on an ephemeral port instead of the
  // default loopback bind, and sets TYMUXD_TOKEN in the spawned process's
  // env — mirroring the Go (`startDaemonWithToken`) and Rust (Story
  // 1.2.2b) non-loopback test harnesses. Omit to keep today's
  // loopback/no-token default behavior unchanged.
  token?: string;
  // Epic 8.3 Task 8.3.1a: when set, sets TYMUXD_SOCKET_PATH in the spawned
  // process's env so tymuxd binds its UDS listener at a known, caller-
  // controlled path — mirroring clients/go's `startDaemonWithUDS` (Task
  // 7.2.1a). When omitted, a path scoped to this call's own stateDir is
  // used instead of leaving TYMUXD_SOCKET_PATH unset: tymuxd now always
  // binds a UDS listener alongside TCP (commit b44aae1), so an unset
  // TYMUXD_SOCKET_PATH would resolve to the ambient, uid-derived default
  // (auth::default_uds_socket_path) — colliding with a real tymuxd this
  // machine may already have running, or with a stale socket dir left
  // behind by one (confirmed: this repo's dev sandbox had exactly such a
  // leftover /run/user/<uid>/tymuxd/ at test-writing time, which made
  // every pre-existing integration test fail with tymuxd's own
  // ensure_socket_parent_dir ownership/mode guard, not a bug in this
  // feature). Every daemon this harness spawns must be as isolated for
  // UDS as it already is for TCP (ephemeral port) and state (XDG_STATE_HOME
  // stateDir).
  socketPath?: string;
}

// Spawns a real tymuxd on an ephemeral loopback port, per this repo's own
// `restart_persistence.rs` pattern of testing against the real binary
// rather than mocking the daemon. Pass `{ token }` to instead bind
// non-loopback with bearer-token auth enforced. Pass `{ socketPath }` to
// pin its UDS listener at a known path (Task 8.3.1a); otherwise one is
// generated inside this call's own isolated stateDir.
export async function startDaemon(options: StartDaemonOptions = {}): Promise<TestDaemon> {
  const { token } = options;
  const port = 20000 + Math.floor(Math.random() * 20000);
  const bindHost = token ? "0.0.0.0" : "127.0.0.1";
  const addr = `${bindHost}:${port}`;
  const stateDir = mkdtempSync(join(tmpdir(), "tymuxd-ts-test-"));
  const socketPath = options.socketPath ?? join(stateDir, "tymuxd.sock");

  const env: NodeJS.ProcessEnv = {
    ...process.env,
    TYMUXD_ADDR: addr,
    XDG_STATE_HOME: stateDir,
    TYMUXD_SOCKET_PATH: socketPath,
  };
  if (token) {
    env.TYMUXD_TOKEN = token;
  } else {
    delete env.TYMUXD_TOKEN;
  }

  const child: ChildProcessByStdio<null, Readable, Readable> = spawn(resolveBinary(), [], {
    env,
    stdio: ["ignore", "pipe", "pipe"],
  });

  await new Promise<void>((resolve, reject) => {
    const timeout = setTimeout(() => reject(new Error("tymuxd did not report listening within 5s")), 5000);
    const onData = (chunk: Buffer) => {
      if (chunk.toString().includes("tymuxd listening")) {
        clearTimeout(timeout);
        child.stdout.off("data", onData);
        resolve();
      }
    };
    child.stdout.on("data", onData);
    child.on("exit", (code) => {
      clearTimeout(timeout);
      reject(new Error(`tymuxd exited early with code ${code}`));
    });
  });

  // The daemon binds bindHost (0.0.0.0 for the token case, so
  // socket_addr.ip().is_loopback() is false and auth is enforced
  // server-side), but a 0.0.0.0 destination isn't reliably connectable
  // from a client across platforms — connect via 127.0.0.1, which a
  // 0.0.0.0 bind also listens on.
  const connectHost = token ? "127.0.0.1" : bindHost;

  return {
    addr: `http://${connectHost}:${port}`,
    stop() {
      child.kill("SIGTERM");
      rmSync(stateDir, { recursive: true, force: true });
    },
  };
}
