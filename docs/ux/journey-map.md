# User Journey Map — tymux
> Generated 2026-07-13. Focus: whole app.
>
> Grounded entirely in the current code (`crates/tymux-cli`, `crates/tymuxd`,
> `crates/tymux-core`, `proto/tymux/v1/tymux.proto`), not the README's
> aspirational framing. tymux is pre-alpha: one session = one window = one
> pane, no auth, no persistence.

## User Types

| Type | Description | Primary activities |
|---|---|---|
| Interactive terminal user | A human running `tymux` at a shell, porting tmux muscle memory | Create/list/attach sessions, interact with a live shell |
| AI coding agent / programmatic client | A script or agent driving tymux over gRPC directly | CapturePane (structured reads), CreateSession, scripted Attach |
| Future web frontend (e.g. stapler-squad) | A web app embedding tymux sessions | Same gRPC surface as an AI agent — no dedicated affordances yet |

## Story Map Backbone

| Activity | Users | Key tasks |
|---|---|---|
| Manage sessions | Interactive user, AI agent | `tymux new`, `tymux ls`, `CreateSession`, `ListSessions`; `KillSession` exists server-side only |
| Attach to and interact with a pane | Interactive user, AI agent | `tymux attach <id>`, send input, receive output over the bidi `Attach` stream; ending = only via shell exit |
| Read pane state structurally (no attach) | AI agent, future web frontend | `CapturePane` → structured cell grid + cursor position, no ANSI parsing |
| Resize a pane | AI agent (works); interactive user (not wired up) | `Resize` message over `Attach`; CLI never sends one on local terminal resize |
| Build/generate a client | AI agent, web frontend, script authors | `buf generate` against `tymux.proto`; `buf lint`/`buf breaking`; connect with no auth |
| Run and operate the daemon | Local operator | `cargo run -p tymuxd`, `TYMUXD_ADDR`, kill = ends every session (no persistence) |

## Journeys

### 1. First session create + attach (human CLI)
**Trigger**: user runs `tymux new`
**Emotional tone**: routine/confident — matches tmux muscle memory, until something goes wrong
**Steps**:
1. CLI connects to the daemon and sends `CreateSessionRequest`
2. Daemon spawns a pane at a **hardcoded 24×80** — the proto has no size field, so the pane is never sized to the caller's real terminal
3. Daemon stores the session and returns it (response's `rows`/`cols` are hardcoded literals, not read from the live pane)
4. CLI grabs the pane id and attaches: raw mode enabled, stdin forwarded on a thread, output streamed to stdout

**Gaps / UX notes**:
- No size field on `CreateSessionRequest`, no initial `Resize` sent at attach time — geometry never matches the user's actual terminal
- Session name uniqueness is never enforced — two `tymux new` calls silently create two distinct sessions both named `default`, unlike tmux which refuses a duplicate name

```mermaid
%%{init: {'theme': 'base', 'themeVariables': {
  'primaryColor': '#1E293B',
  'primaryTextColor': '#F1F5F9',
  'primaryBorderColor': '#334155',
  'lineColor': '#64748B',
  'edgeLabelBackground': '#F8FAFC',
  'fontFamily': 'ui-sans-serif, system-ui',
  'fontSize': '14px'
}}}%%
stateDiagram-v2
    [*] --> Disconnected

    Disconnected --> Connected: connect(addr)
    Connected --> SessionRequested: send CreateSessionRequest
    SessionRequested --> PaneSpawned: spawn pane 24x80
    PaneSpawned --> SessionStored: insert into HashMap
    SessionStored --> SessionReady: return Session + id

    SessionReady --> Attaching: call attach(pane id)
    Attaching --> RawModeEnabled: enable raw mode
    RawModeEnabled --> StreamsWired: stdin/stdout wired

    StreamsWired --> Interacting: shell live
    Interacting --> Interacting: user types commands

    Interacting --> [*]: detach or exit
```

---

### 2. Exiting the shell inside an attached pane (Ctrl-d) — ⚠️ real bug, not just a gap
**Trigger**: user types `exit`/Ctrl-D expecting the README's documented clean return to the outer shell
**Emotional tone**: confused → frustrated — directly contradicts documented behavior, and is likely a new user's first-five-minutes experience
**Steps**:
1. Child process exits; the pty reader thread gets `Ok(0)` and breaks — but signals nothing
2. The pane's output `broadcast::Sender` is never dropped (the `Pane` is still alive in the engine map), so the daemon's forwarding task **blocks forever** awaiting a message that will never arrive
3. There is no `AttachEvent` variant for "pane exited"/EOF/exit code — the protocol has no way to say this happened
4. The CLI's `inbound.message().await` therefore also blocks forever; the terminal is stuck in raw mode showing a stale screen
5. Further keystrokes are forwarded to the dead pty; write errors are silently discarded
6. The only way out is externally killing the CLI process — meanwhile `tymux ls` reports the session as alive forever

**Gaps / UX notes**:
- No child-exit detection anywhere (no `wait()`/`try_wait()`/SIGCHLD handling)
- No liveness/status field anywhere in the protocol
- This is a genuine dead end today — worth fixing before anything else in this list

```mermaid
%%{init: {'theme': 'base', 'themeVariables': {
  'primaryColor': '#1E293B',
  'primaryTextColor': '#F1F5F9',
  'primaryBorderColor': '#334155',
  'lineColor': '#64748B',
  'edgeLabelBackground': '#F8FAFC',
  'fontFamily': 'ui-sans-serif, system-ui',
  'fontSize': '14px'
}}}%%
stateDiagram-v2
    [*] --> Attached

    Attached: Attached, expects clean exit
    note right of Attached
      User types exit / Ctrl-D
      Expects README behavior
    end note
    Attached --> ChildExited: Ctrl-D / exit

    ChildExited: Child process exits
    ChildExited --> ReaderBroke: pty read returns Ok(0)

    ReaderBroke: Reader thread breaks silently
    note right of ReaderBroke
      No signal sent to anyone
    end note
    ReaderBroke --> SenderAlive: Sender never dropped

    SenderAlive: Pane stays in engine map
    SenderAlive --> ForwarderBlocked: broadcast Sender alive

    ForwarderBlocked: Daemon forwarder awaits forever
    ForwarderBlocked --> ProtocolGap: no message arrives

    ProtocolGap: No exit AttachEvent variant
    note right of ProtocolGap
      Protocol cannot express
      pane exited / EOF / exit code
    end note
    ProtocolGap --> CliBlocked: nothing to send CLI

    CliBlocked: CLI inbound.message await blocks
    CliBlocked --> StaleRawMode: local terminal stuck

    StaleRawMode: Stale screen, raw mode stuck
    StaleRawMode --> KeystrokesDropped: user presses keys

    KeystrokesDropped: Keystrokes sent to dead pty
    note right of KeystrokesDropped
      Write errors silently discarded
    end note
    KeystrokesDropped --> Hung: still no response

    Hung: Hung, no exit path
    note right of Hung
      BUG: no exit today
      Only fix: kill CLI process externally
      tymux ls still reports session alive
    end note
    KeystrokesDropped --> Hung
    StaleRawMode --> Hung: waiting forever

    classDef normal fill:#3B82F6,stroke:#1D4ED8,color:#fff,stroke-width:2px
    classDef warning fill:#F59E0B,stroke:#92400E,color:#fff,stroke-width:2px
    classDef danger fill:#EF4444,stroke:#991B1B,color:#fff,stroke-width:2px

    class Attached normal
    class ChildExited,ReaderBroke,SenderAlive normal
    class ForwarderBlocked,ProtocolGap warning
    class CliBlocked,StaleRawMode,KeystrokesDropped warning
    class Hung danger
```

---

### 3. Detaching from a session without ending it (missing feature)
**Trigger**: user wants tmux's `Ctrl-b d` — leave the pane running, return to their own shell
**Emotional tone**: trapped — no designed exit, only violent/accidental ones
**Steps**:
1. Raw mode + unconditional stdin forwarding means every byte (Ctrl-C, Ctrl-D, Ctrl-\\) goes to the **remote** pty, never intercepted locally
2. No prefix key, `--detach` flag, or signal handler exists anywhere
3. Only escape hatches: force-kill the local `tymux` process, or close the terminal emulator (SIGHUP)
4. The remote session mechanically survives either way, but the user gets no confirmation — they have to run `tymux ls` and hope
5. Reattaching works, but only if the user still has the raw session UUID

**Gaps / UX notes**:
- No detach primitive of any kind — a hard usability floor for a "tmux-inspired" tool
- No name-based or "most recent session" attach convenience; must copy/paste a UUID

```mermaid
%%{init: {'theme': 'base', 'themeVariables': {
  'primaryColor': '#1E293B',
  'primaryTextColor': '#F1F5F9',
  'primaryBorderColor': '#334155',
  'lineColor': '#64748B',
  'edgeLabelBackground': '#F8FAFC',
  'fontFamily': 'ui-sans-serif, system-ui',
  'fontSize': '14px'
}}}%%
flowchart TD
  Start(["Attached to pane"]) --> Want["Wants to detach\nkeep session running"]
  Want --> Decision{"Prefix key or\ndetach flag exists?"}

  Decision -- "NO" --> Missing["No detach path\n(missing)"]
  Missing --> RawMode["Raw mode: all\nstdin forwarded"]
  RawMode --> Bytes["Ctrl-C / Ctrl-D / Ctrl-\\\ngo to remote pty"]
  Bytes --> Escape{"Only escape hatches"}

  Escape --> ForceKill["Force-kill\nlocal tymux"]
  Escape --> CloseTerm["Close terminal\n(SIGHUP)"]

  ForceKill --> Survives["Remote session\nsurvives silently"]
  CloseTerm --> Survives

  Survives --> NoConfirm["No confirmation\ngiven to user"]
  NoConfirm --> Assumes["User only assumes\nit worked"]

  Assumes --> HasUUID{"Still has\nsession UUID?"}
  HasUUID -- "Yes" --> Reattach["tymux ls\n+ reattach works"]
  HasUUID -- "No" --> Lost["Session orphaned\nunreachable"]

  classDef start fill:#3B82F6,stroke:#1D4ED8,color:#fff,stroke-width:2px
  classDef danger fill:#EF4444,stroke:#991B1B,color:#fff,stroke-width:2px
  classDef warning fill:#F59E0B,stroke:#92400E,color:#fff,stroke-width:2px
  classDef ghost fill:#F9FAFB,stroke:#D1D5DB,color:#374151,stroke-dasharray:5 5
  classDef success fill:#10B981,stroke:#065F46,color:#fff,stroke-width:2px

  class Start,Want start
  class Missing danger
  class RawMode,Bytes,Escape,ForceKill,CloseTerm warning
  class Survives,NoConfirm,Assumes,Lost danger
  class Reattach success
  class HasUUID ghost
```

---

### 4. Killing a session (human CLI)
**Trigger**: user wants to end a session's process entirely
**Emotional tone**: human — confused there's no kill command; programmatic — worse, since the RPC reports success while cleanup isn't actually verifiable
**Steps**:
1. `KillSession` is fully implemented server-side (removes the session from the daemon's map)
2. `tymux-cli`'s command enum only has `New`/`Ls`/`Attach` — **no `kill` subcommand exists** to invoke it
3. Even called directly, an actively-attached client still holds its own `Arc<Pane>` clone, so the process survives until that client disconnects
4. `Pane` has no `Drop` impl and nothing calls `.kill()` on the child handle — the OS process may leak even after the last reference is dropped

**Gaps / UX notes**:
- Missing CLI subcommand for a fully-implemented, documented RPC
- `KillSession`'s name promises termination the code doesn't verifiably deliver

```mermaid
%%{init: {'theme': 'base', 'themeVariables': {
  'primaryColor': '#1E293B',
  'primaryTextColor': '#F1F5F9',
  'primaryBorderColor': '#334155',
  'lineColor': '#64748B',
  'edgeLabelBackground': '#F8FAFC',
  'fontFamily': 'ui-sans-serif, system-ui',
  'fontSize': '14px'
}}}%%
flowchart TD
  Start(["User wants to kill session"])

  Start --> ServerCheck{"RPC exists server-side?"}

  ServerCheck -->|"Yes"| RpcImpl["KillSession RPC implemented"]
  RpcImpl --> RemoveHashMap["Removes SessionState from HashMap"]

  ServerCheck -->|"CLI path"| CliCheck{"Does CLI expose kill?"}
  CliCheck -->|"No"| NoSubcommand["No kill subcommand exists"]
  NoSubcommand --> HumanConfused["Human confused: no kill command?"]
  HumanConfused --> HumanStuck(["Human cannot kill session"]):::danger

  CliCheck -.->|"bypass CLI"| RawGrpc["Raw gRPC call to KillSession"]
  RawGrpc --> RemoveHashMap

  RemoveHashMap --> RpcReportsSuccess["RPC reports success"]
  RpcReportsSuccess --> ClientCheck{"Any client still attached?"}

  ClientCheck -->|"Yes"| ArcHeld["Attached client holds Arc clone of Pane"]
  ArcHeld --> PaneSurvives["Pane/child process survives removal"]
  PaneSurvives --> WaitDisconnect["Waits for client disconnect"]
  WaitDisconnect --> LastArcDropped["Last Arc reference dropped"]

  ClientCheck -->|"No"| LastArcDropped

  LastArcDropped --> DropCheck{"Drop impl on Pane?"}
  DropCheck -->|"None"| NoKillCall["Nothing calls kill() on child"]
  NoKillCall --> MayLeak(["Process may leak: no Drop, no kill()"]):::warning

  AiCaller(["AI client calls KillSession directly"]):::ghost
  AiCaller -.-> RpcReportsSuccess
  RpcReportsSuccess -.->|"misleading"| MisleadingSuccess["Success reported but cleanup unverifiable"]
  MisleadingSuccess -.-> MayLeak

  classDef danger  fill:#EF4444,stroke:#991B1B,color:#fff,stroke-width:2px
  classDef warning fill:#F59E0B,stroke:#92400E,color:#fff,stroke-width:2px
  classDef ghost   fill:#F9FAFB,stroke:#D1D5DB,color:#374151,stroke-dasharray:5 5

  linkStyle default stroke:#94A3B8,stroke-width:1.5px
```

---

### 5. AI agent reads pane state via CapturePane
**Trigger**: a programmatic/AI client wants a one-shot structured screen read — the project's headline differentiator over `tmux capture-pane`
**Emotional tone**: predictable, synchronous, well-typed — **the best-designed flow in the codebase**
**Steps**:
1. Client sends `CapturePaneRequest{pane_id}`
2. Daemon parses the UUID and looks up the pane
3. Locks the vt100 parser and builds a full structured grid (cells with text + packed fg/bg/attrs) plus cursor position
4. Returns the structured `PaneSnapshot` — no ANSI re-parsing needed, genuinely works as advertised

**Gaps / UX notes**:
- No liveness field — a dead pane's last snapshot looks identical to an idle live one
- No sequence number/dirty-region info — every call retransmits the entire grid, chatty for a polling agent
- `pane_id`/`session_id` are both untyped strings — mixing them up produces a generic, unhelpful `not_found`

```mermaid
%%{init: {'theme': 'base', 'themeVariables': {
  'primaryColor': '#1E293B',
  'primaryTextColor': '#F1F5F9',
  'primaryBorderColor': '#334155',
  'lineColor': '#64748B',
  'edgeLabelBackground': '#F8FAFC',
  'fontFamily': 'ui-sans-serif, system-ui',
  'fontSize': '14px'
}}}%%
stateDiagram-v2
    [*] --> RequestSent

    RequestSent: Client sends CapturePaneRequest
    ParsingID: Daemon parses pane ID
    LocatingPane: Engine looks up pane
    LockingParser: Locks vt100 parser
    BuildingGrid: Builds structured grid
    SnapshotReturned: Returns PaneSnapshot
    ConsumingCells: Client reads typed cells

    RequestSent --> ParsingID
    ParsingID --> LocatingPane
    LocatingPane --> LockingParser
    LockingParser --> BuildingGrid
    BuildingGrid --> SnapshotReturned
    SnapshotReturned --> ConsumingCells
    ConsumingCells --> [*]

    note right of ConsumingCells
        Caveat: a dead pane's last
        snapshot looks identical to
        an idle live one - no
        liveness field exists
    end note
```

---

### 6. Multiple clients attach to the same pane
**Trigger**: a second human/AI client attaches to a `pane_id` that already has an active attacher — core tmux behavior, and the code does not reject it
**Emotional tone**: silently corrupted/confusing rendering for a human, with no error message; a nondeterministic race for programmatic mixed attach+resize clients
**Steps**:
1. Output fan-out to N clients works correctly (broadcast channel supports multiple receivers by design)
2. Both clients' input interleaves into the same shell — matches tmux's real behavior, not a bug
3. **Resize is not arbitrated at all** — each client can independently resize the shared pty; last write wins, with no comparison against other attachers and no notification back to them

**Gaps / UX notes**:
- No shared/arbitrated pty size across concurrent attachers
- No read-only attach mode; no visibility (via `ListSessions`) into how many clients are already attached

```mermaid
%%{init: {'theme': 'base', 'themeVariables': {
  'primaryColor': '#1E293B',
  'primaryTextColor': '#F1F5F9',
  'primaryBorderColor': '#334155',
  'lineColor': '#64748B',
  'edgeLabelBackground': '#F8FAFC',
  'fontFamily': 'ui-sans-serif, system-ui',
  'fontSize': '14px'
}}}%%
flowchart TD
  Start(["Pane X active"])

  subgraph laneA["Client A"]
    direction TB
    A1["Attach to pane X"]
    A2["Daemon creates subscriber A"]
    A3["Output fan-out begins"]
    A4["Send keystrokes"]
    A5["Send Resize rows/cols"]
  end

  subgraph laneB["Client B (concurrent)"]
    direction TB
    B1["Attach to SAME pane X"]
    B2["No existing-attach check"]
    B3["Daemon creates subscriber B"]
    B4["Output fan-out begins"]
    B5["Send keystrokes"]
    B6["Send different Resize"]
  end

  Writer["Mutex-protected pty writer"]
  Interleaved["Interleaved input (tmux-like)"]
  SharedSize["Shared pane size (last write wins)"]
  NoVis["ListSessions: no attach count"]
  Result(["Silent nondeterministic rendering, no error"])

  Start --> A1 --> A2 --> A3
  Start --> B1 --> B2 --> B3 --> B4

  A3 --> A4
  B4 --> B5
  A4 --> Writer
  B5 --> Writer
  Writer --> Interleaved

  A3 --> A5
  B4 --> B6
  A5 --> SharedSize
  B6 --> SharedSize
  SharedSize --> Result
  NoVis -.no arbitration visibility.-> Result

  classDef clientA fill:#3B82F6,stroke:#1D4ED8,color:#fff,stroke-width:2px
  classDef clientB fill:#F59E0B,stroke:#92400E,color:#fff,stroke-width:2px
  classDef shared fill:#7C3AED,stroke:#4C1D95,color:#fff,stroke-width:2px
  classDef danger fill:#EF4444,stroke:#991B1B,color:#fff,stroke-width:2px
  classDef ghost fill:#F9FAFB,stroke:#D1D5DB,color:#374151,stroke-dasharray:5 5

  class A1,A2,A3,A4,A5 clientA
  class B1,B2,B3,B4,B5,B6 clientB
  class Writer,Interleaved,SharedSize shared
  class NoVis ghost
  class Result danger

  style laneA fill:#EFF6FF,stroke:#BFDBFE
  style laneB fill:#FFFBEB,stroke:#FDE68A

  linkStyle 14 stroke:#EF4444,stroke-width:2px
  linkStyle 15 stroke:#EF4444,stroke-width:2px
  linkStyle 16 stroke:#EF4444,stroke-width:2px
  linkStyle 17 stroke:#EF4444,stroke-dasharray:5 5
```

---

### 7. Terminal resize (SIGWINCH) during an attached session
**Trigger**: user resizes their terminal emulator window while attached
**Emotional tone**: confusing — resizing the window silently does nothing remotely
**Steps**: no SIGWINCH handler and no `crossterm` resize polling exist anywhere in the CLI; the initial `attach()` call never even sends one `Resize` frame to sync the pane to the client's real starting size.

**Gaps / UX notes**:
- Combined with Flow 1's gap, pane size can never be made to match the user's real terminal through the CLI as written — every session is permanently 24×80 unless a client hand-crafts a `Resize` frame itself

*(No diagram generated — same root cause as Flow 1, folded into the cross-cutting geometry gap below.)*

---

### 8. Daemon not running when the CLI tries to connect
**Trigger**: user runs any `tymux` subcommand before starting `tymuxd` — likely for any first-time user, since the README shows two separate manual commands
**Emotional tone**: confusing technical error dump instead of an actionable message
**Steps**: connection fails, propagates via `anyhow` to `main()`, and prints a raw `Debug` dump of the transport error chain.

**Gaps / UX notes**:
- No friendly "is tymuxd running?" message; no auto-start of the daemon (tmux itself auto-forks a server)
- Identical unfriendly failure mode across `ls`/`new`/`attach`

*(No diagram — this is a single error-formatting gap, not a multi-step flow worth a state diagram.)*

---

### 9. Attaching to a session by ID (reattach)
**Trigger**: `tymux attach <session_id>`, e.g. after an involuntary detach (Flow 3) or to join a session an AI agent created
**Emotional tone**: minor friction typing a UUID by hand, but the failure mode itself is comparatively clean
**Steps**: CLI fetches *all* sessions via `ListSessions` and does a client-side linear find by id, then attaches. A kill-between-list-and-attach race is handled correctly (terminal state is restored either way).

**Gaps / UX notes**:
- No name-based attach despite names being displayed by `ls`; no "most recent session" convenience
- Wasteful full `ListSessions` round trip just to validate one ID the server could look up directly

*(No diagram — a single linear path with one already-handled edge case, not diagram-worthy on its own.)*

---

### 10. Daemon restart / crash recovery
**Trigger**: `tymuxd` killed or crashes while sessions exist (explicitly out of scope per the README)
**Emotional tone**: programmatic clients get a standard, pattern-matchable gRPC `Unavailable` status; human CLI users get the same ugly generic error as Flow 8
**Steps**: all in-memory engine state disappears with the process; attached clients' streams immediately error.

**Gaps / UX notes**:
- No reconnect/resume concept in the wire protocol at all — worth naming explicitly now, since it's the natural next gap once persistence is ever added

*(No diagram — acknowledged, out-of-scope-by-design gap; documented here for completeness rather than as a surprise finding.)*

## Cross-Cutting Gaps

1. **No liveness/status signal anywhere in the protocol.** Neither `Session`, `Window`, `Pane`, `PaneSnapshot`, nor `AttachEvent` carries an "is this alive"/"did it exit"/"exit code" field. This single missing field is the root cause of Flow 2's hang, half of Flow 5's ambiguity, and part of Flow 4's unverifiable cleanup. **Highest-leverage fix in this whole map.**
2. **Every CLI failure path funnels into `anyhow`'s raw `Debug` print.** No unified human-friendly error layer exists — connection-refused, session-not-found, and pane-not-found all get the same technical dump (Flows 8, 10).
3. **The CLI's command surface is a strict subset of the daemon's capability.** `KillSession` is fully implemented server-side and entirely unreachable from the `tymux` binary (Flow 4).
4. **Terminal geometry is hardcoded and never synchronized.** No size param on `CreateSession`, no initial `Resize` on attach, no SIGWINCH wiring, and `ListSessions`/`CreateSession` responses hardcode stale `rows:24, cols:80` (Flows 1, 5, 7).
5. **No detach primitive.** Raw mode forwards every byte to the remote pty with no prefix key or signal escape hatch (Flow 3).
6. **No concurrency arbitration for shared panes.** Multiple attachers are allowed (correctly matching tmux) but resize is a free-for-all with no attach-count visibility or read-only mode (Flow 6).
7. **Session/pane identity is weak.** No name uniqueness, no partial-match or "most recent" attach convenience, `pane_id`/`session_id` are interchangeable untyped strings, and there's no dedicated window/pane-listing RPC — both the CLI and any programmatic client hardcode `windows[0].panes[0]`, which breaks the moment splits or multiple windows exist (Flows 1, 5, 9; also noted independently by the story-map pass).
8. **No rename, no splits, no multiple windows yet.** `SessionState` is hardcoded to one pane per session today — the proto already models `repeated windows`/`repeated panes`, so this is additive work, not a breaking change, whenever it's tackled.

## Documentation Opportunities

- **"Known Limitations" page** — one canonical list of the MVP gaps above (no detach, no auth, no persistence, hardcoded geometry), so users don't discover them by surprise the way Flow 2 currently does
- **"Troubleshooting: connecting to tymuxd"** — covers Flows 8 and 10's raw error dumps until the error-formatting gap is fixed
- **"CLI command reference"** — once `kill` (and any future commands) exist, a single source of truth distinct from the daemon's full RPC surface
- **"Building a programmatic client"** — a short guide for the AI-agent/web-frontend audience covering `CreateSession` → `CapturePane`/`Attach`, written around Flow 5 (the strongest flow today) as the model example
- **"Roadmap"** — detach/reattach, `kill` subcommand, terminal-geometry sync, and liveness signaling are natural, concretely-scoped next milestones straight out of this map
