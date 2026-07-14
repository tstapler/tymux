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

// Spawns a real tymuxd on an ephemeral loopback port, per this repo's own
// `restart_persistence.rs` pattern of testing against the real binary
// rather than mocking the daemon.
export async function startDaemon(): Promise<TestDaemon> {
  const port = 20000 + Math.floor(Math.random() * 20000);
  const addr = `127.0.0.1:${port}`;
  const stateDir = mkdtempSync(join(tmpdir(), "tymuxd-ts-test-"));

  const child: ChildProcessByStdio<null, Readable, Readable> = spawn(resolveBinary(), [], {
    env: { ...process.env, TYMUXD_ADDR: addr, XDG_STATE_HOME: stateDir },
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

  return {
    addr: `http://${addr}`,
    stop() {
      child.kill("SIGTERM");
      rmSync(stateDir, { recursive: true, force: true });
    },
  };
}
