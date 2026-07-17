use std::time::Duration;

use tymux_e2e::harness::CliHarness;
use tymux_e2e::{daemon, workspace_bin};
use tymux_proto::v1::CreateSessionRequest;

/// Proves the golden-screen framework: drive a deterministic sequence
/// (a fixed `/bin/sh` command with a pinned `PS1`, not the user's real
/// `$SHELL`, so this never depends on a fancy interactive prompt or
/// locale-specific output) and snapshot the rendered screen text via
/// `insta`. A real visual regression — e.g. a DECSTBM scroll-region bug
/// that shifts content by a row, or the status bar's session-name text
/// changing format — shows up as a snapshot diff on the next
/// `cargo test` run; `cargo insta review` (or `INSTA_UPDATE=always
/// cargo test`) accepts an intentional change.
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
    // Let the shell finish redrawing its prompt after the command
    // completes, so the snapshot captures a settled screen.
    std::thread::sleep(Duration::from_millis(300));

    insta::assert_snapshot!(h.screen_text());

    h.detach(Duration::from_secs(3));
}
