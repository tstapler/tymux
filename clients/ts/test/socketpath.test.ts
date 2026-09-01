import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { defaultSocketPath, resolveSocketPath } from "../examples/client.js";

// Shared cross-language fixture (Phase 1, commit 5537fa6) — see
// architecture-review.md's test-duplication-drift Concern fix: every
// implementation's test suite reads this one file rather than
// independently hand-typing the same Given/When/Then cases. It lives in
// testdata/ at the repo root, not project_plans/, since two of the four
// consumers read it via include_str! at Rust compile time.
const FIXTURES_PATH = fileURLToPath(
  new URL("../../../testdata/unix-socket-auth/socket-path-fixtures.json", import.meta.url),
);

interface DefaultPathCase {
  case: string;
  env: Record<string, string>;
  uid: number;
  expected: string;
}

interface ResolvePathCase {
  case: string;
  args: string[];
  env: Record<string, string>;
  uid: number;
  expected: string;
}

interface Fixtures {
  default_path_cases: DefaultPathCase[];
  resolve_path_cases: ResolvePathCase[];
}

const fixtures: Fixtures = JSON.parse(readFileSync(FIXTURES_PATH, "utf8"));

// Runs `fn` with only the named env vars set (XDG_RUNTIME_DIR/TMPDIR/
// TYMUXD_SOCKET_PATH), restoring the previous values afterward — the
// fixture cases rely on the *absence* of a var, not just its value, so an
// inherited value from the running shell must not leak in.
function withEnv<T>(env: Record<string, string>, fn: () => T): T {
  const keys = ["XDG_RUNTIME_DIR", "TMPDIR", "TYMUXD_SOCKET_PATH"] as const;
  const saved = new Map<string, string | undefined>(keys.map((k) => [k, process.env[k]]));
  try {
    for (const k of keys) delete process.env[k];
    for (const [k, v] of Object.entries(env)) process.env[k] = v;
    return fn();
  } finally {
    for (const k of keys) {
      const v = saved.get(k);
      if (v === undefined) delete process.env[k];
      else process.env[k] = v;
    }
  }
}

for (const c of fixtures.default_path_cases) {
  test(`defaultSocketPath: ${c.case}`, () => {
    const actual = withEnv(c.env, () => defaultSocketPath(c.uid));
    assert.equal(actual, c.expected);
  });
}

// `resolveSocketPath(uid)` takes no `args` parameter — unlike
// tymuxd/tymux-cli's Rust `resolve_uds_socket_path`, this is a library
// function with no CLI-flag layer of its own (mirroring clients/go's
// `ResolveSocketPath`, Task 7.1.1b), so only the fixture cases that don't
// depend on a `--socket-path` flag apply here.
for (const c of fixtures.resolve_path_cases.filter((c) => c.args.length === 0)) {
  test(`resolveSocketPath: ${c.case}`, () => {
    const actual = withEnv(c.env, () => resolveSocketPath(c.uid));
    assert.equal(actual, c.expected);
  });
}
