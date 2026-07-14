# v1.0.0 release verification checklist

Story 8.3 (plan.md): executed against the actual tagged release binaries
(downloaded from the GitHub Release), not `cargo run`, before the real
`v1.0.0` tag is cut. One item per `requirements.md` Success Metric / in-scope
item.

## Prerequisites
- [ ] `v1.0.0` tag pushed, `release.yml` completed successfully
- [ ] All four archives (`x86_64-unknown-linux-musl`,
      `aarch64-unknown-linux-musl`, `x86_64-apple-darwin`,
      `aarch64-apple-darwin`) present on the GitHub Release
- [ ] Download and run at least one archive per OS (Linux + macOS) without a
      Rust toolchain installed on the test machine

## Checklist

- [ ] **Splits**: `tymux new`, then `tymux split <target> --vertical` and
      `--horizontal`; confirm both panes render independently and each
      accepts input separately (Success Metric: multi-pane sessions)
- [ ] **Persistence**: create a session with a couple of panes, kill
      `tymuxd`, restart it, `tymux ls` shows the session as dead-but-listed,
      `tymux revive <session>` respawns panes in the original layout/cwd
      (Success Metric: restart does not lose sessions)
- [ ] **Cross-language client**: from `clients/ts/`, run
      `npm run list-sessions` / the `Attach` example against the released
      `tymuxd` binary; confirm `CreateSession`, `Attach`, `CapturePane` all
      work (Success Metric: non-Rust client proof)
- [ ] **Scrollback/copy-mode**: generate scrollback (e.g. `seq 1 200`),
      enter copy-mode (`C-b [`), scroll back, search, copy a selection
      (Success Metric: scrollback capture + interactive navigation)
- [ ] **Status bar**: attach to a session, confirm the status bar shows
      current session/window state; press the prefix key and confirm the
      hint line appears; verify `--no-status-bar` disables it entirely
- [ ] **Config + keybindings**: write a `config.toml` overriding at least
      one binding, confirm it takes effect; confirm the default detach
      keybinding (`C-b d`) works with no config file present
- [ ] **vim / DECSTBM visual check** (Story 6.4's flagged manual spike —
      not automatable): run `vim` inside a pane with the status bar
      enabled, confirm no scroll-region corruption when vim redraws
      full-screen
- [ ] **README cross-language claim**: confirm `clients/ts/README.md`'s
      quick-start works verbatim against a released binary, without reading
      Rust source first

## Sign-off
- [ ] Every item above passes against the tagged release binaries
- [ ] `v1.0.0` tag cut only after all items pass
