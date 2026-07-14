# Product Marketing Context
*Type: open-source*
*Last updated: 2026-07-13*

## Project Overview
**One-liner:** tmux's session model, rebuilt with a typed gRPC API — for terminals, AI agents, and web frontends alike.
**What it does:** tymux reimplements tmux's session/window/pane model in Rust with a real PTY engine (portable-pty + vt100), but exposes it over gRPC/protobuf instead of tmux's text-based control mode. Any client — CLI, web UI, AI agent — drives sessions through typed RPCs and gets structured screen state back, instead of scraping ANSI output.
**Category:** terminal multiplexer / PTY session engine / dev tool
**Type:** CLI + daemon (client/server)
**License:** MIT (per `Cargo.toml` — a `LICENSE` file still needs to be added to the repo)

## Audience
**Primary users:** developers who want scriptable, programmatic control over terminal sessions — including AI coding-agent orchestrators (e.g. stapler-squad) that need to spawn and read terminal sessions without ANSI-scraping.
**Secondary users:** tmux power users curious about a structured-API-first alternative; anyone building a web-based terminal UI who doesn't want to hand-roll PTY plumbing.
**Contributors:** Rust systems/terminal enthusiasts, people who've hit tmux control-mode's text-parsing wall themselves, gRPC/protobuf-comfortable developers who want to add non-Rust clients.
**Not for:** people wanting a polished GUI terminal app out of the box, or a drop-in tmux config/plugin replacement today (no split panes, no config file, no plugin ecosystem yet).

## Problem & Differentiation
**The problem:** tmux's scripting surface (`capture-pane`, control mode) hands you raw text — ANSI escapes included — that has to be re-parsed to know what's actually on screen. There's no first-class structured API, and no clean cross-language story for driving a multiplexer from anything other than shell scripts.
**Alternatives fall short because:** tmux control mode means scraping text; `screen` is even less scriptable; hand-rolled PTY-wrapping scripts solve it once, per-project, per-language, with no shared schema.
**Core philosophy:** structured screen state (cells + attributes) over gRPC is the primary interface, not a bolt-on afterthought — proto-first design, buf-managed schema, cross-language by construction.
**Word-of-mouth pitch:** "It's tmux, but you drive it like an API — structured screen state over gRPC, not scraped ANSI text."

## Brand Voice
**Personality:** pragmatic, direct, technically confident, unsentimental, a bit opinionated
**Technical depth:** expert-first (assumes Rust/gRPC/terminal familiarity)
**Writing style:** terse and precise
**Use:** "structured", "typed", "daemon", "cross-language", "session/window/pane"
**Avoid:** hype language ("seamless", "revolutionary", "game-changing", "blazingly fast" as a throwaway claim)
**Voice example:** "CapturePane returns typed cells, not ANSI you have to re-parse."

## Visual Direction
**Color mood:** dark + technical, grimdark accent (warmer/aged than typical neon-cyberpunk dev tooling)
**Colors:** pulled from stapler-squad's "grimdark" (WH40K) theme — near-black background `#0c0a08`, aged-gold primary `#c0a020` (hover `#d4b424`, active `#a08818`), parchment/tan text `#c8b89a`, blood-red accent `#8b1a1a`, muted bronze borders `#3d3020`
**Typography:** monospace-forward
**Aesthetic:** terminal-native — references like tmux/alacritty/wezterm docs, or Charm's tools (Bubbletea, Glow) — filtered through the grimdark gold/near-black/blood-red palette above rather than a cooler cyberpunk-blue treatment
**Logo:** fractal / recursive-split motif — horizontal and vertical pane-splitting evoked as a Fibonacci-spiral or golden-ratio rectangle subdivision, rendered in the grimdark palette. Icon-only or combination mark; in progress via `ui-logo-designer`

## Adoption Goals
**Primary metric:** GitHub stars and dependents (crates.io reverse deps / repos importing `tymux-proto`)
**Discovery path:** GitHub repo, r/rust and r/commandline, HN "Show HN" once there's a demo-able attach flow
**Trust signals:** real CI (fmt/clippy/test, buf lint), a working demo GIF/asciinema of `tymux new` actually attaching, clear architecture docs explaining the structured-state bet
**Adoption barrier:** requires running a daemon (`tymuxd`) rather than a single static binary like tmux — worth addressing head-on in the README rather than hiding it
**"Aha" moment:** the first time a script or AI agent reads `CapturePane`'s structured grid instead of regexing ANSI output, and it just works

## Key Messages
**Headline:** tmux's model, rebuilt with a typed API.
**Supporting:**
- Structured pane capture (cells + attributes) instead of raw ANSI text
- One proto schema, buf-managed — add a TS/Python/Go client without touching the Rust core
- Built PTY-up for programmatic and AI-agent control, not retrofitted onto a human-scripting tool
**CTA:** clone, `cargo run -p tymuxd` + `cargo run -p tymux-cli -- new`, read the architecture section in the README

## GitHub Presence
**README purpose:** narrative intro (the "why rebuild tmux" pitch) up top, quick start right after
**Social proof:** none yet — new project; revisit once there are stars/dependents/a demo to point to
**Contribution posture:** welcoming — aiming for public adoption, so issues/PRs from newcomers should be encouraged, not gatekept
**Topics/tags:** rust, tmux, terminal-multiplexer, grpc, protobuf, pty, developer-tools
