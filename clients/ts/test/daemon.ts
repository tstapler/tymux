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
}

// Spawns a real tymuxd on an ephemeral loopback port, per this repo's own
// `restart_persistence.rs` pattern of testing against the real binary
// rather than mocking the daemon. Pass `{ token }` to instead bind
// non-loopback with bearer-token auth enforced.
export async function startDaemon(options: StartDaemonOptions = {}): Promise<TestDaemon> {
  const { token } = options;
  const port = 20000 + Math.floor(Math.random() * 20000);
  const bindHost = token ? "0.0.0.0" : "127.0.0.1";
  const addr = `${bindHost}:${port}`;
  const stateDir = mkdtempSync(join(tmpdir(), "tymuxd-ts-test-"));

  const env: NodeJS.ProcessEnv = { ...process.env, TYMUXD_ADDR: addr, XDG_STATE_HOME: stateDir };
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
