use std::time::Duration;

use tymux_e2e::harness::CliHarness;
use tymux_e2e::{daemon, workspace_bin};
use tymux_proto::v1::CreateSessionRequest;

/// Proves the golden-screen framework: drive a deterministic sequence (a
/// fixed `/bin/sh` command, not the user's real `$SHELL`, so this never
/// depends on a fancy interactive prompt or locale-specific output) and
/// snapshot the rendered screen text via `insta`. A real visual
/// regression — e.g. a DECSTBM scroll-region bug that shifts content by a
/// row — shows up as a snapshot diff on the next `cargo test` run;
/// `cargo insta review` (or `INSTA_UPDATE=always cargo test`) accepts an
/// intentional change.
///
/// Snapshots only the command's own output lines, not the surrounding
/// shell prompt: `/bin/sh`'s prompt rendering (whether it appears at all,
/// its exact glyph, whether `$PS1` is honored for a non-login shell)
/// genuinely differs across environments — confirmed by this test failing
/// in CI (no visible prompt) despite passing locally (dash renders `$ `)
/// — and that variance isn't the thing this test is trying to catch.
#[tokio::test]
async fn attach_screen_after_deterministic_output_matches_golden_snapshot() {
    let tymuxd_bin = workspace_bin("tymuxd");
    let tymux_bin = workspace_bin("tymux");
    let d = daemon::spawn(&tymuxd_bin);
    let mut client = d.wait_ready().await;
    client
        .create_session(CreateSessionRequest {
            name: "golden-e2e".into(),
            command: "/bin/sh".into(),
        })
        .await
        .unwrap();

    let h = CliHarness::spawn(
        &tymux_bin,
        &[
            "--addr",
            &format!("http://{}", d.addr),
            "--no-status-bar",
            "attach",
            "golden-e2e:0.0",
        ],
        &[],
        10,
        40,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(500));

    h.send_str("printf 'GOLDEN_LINE_1\\nGOLDEN_LINE_2\\n'\n");
    assert!(h.wait_for("GOLDEN_LINE_2", Duration::from_secs(5)));
    // Let the shell finish settling after the command completes, so the
    // snapshot captures a stable screen.
    std::thread::sleep(Duration::from_millis(300));

    let screen = h.screen_text();
    let lines: Vec<&str> = screen.lines().collect();
    // Exact-match on the trimmed line, not substring containment: the
    // *echoed typed command* ("printf 'GOLDEN_LINE_1\nGOLDEN_LINE_2\n'")
    // also contains both substrings, on one line, before the shell ever
    // executes it — only printf's real output renders each marker alone
    // on its own line.
    let start = lines
        .iter()
        .position(|l| l.trim() == "GOLDEN_LINE_1")
        .expect("GOLDEN_LINE_1 should be on screen as its own output line");
    let end = lines
        .iter()
        .position(|l| l.trim() == "GOLDEN_LINE_2")
        .expect("GOLDEN_LINE_2 should be on screen as its own output line");
    let deterministic_slice = lines[start..=end].join("\n");

    insta::assert_snapshot!(deterministic_slice);

    h.detach(Duration::from_secs(3));
}
