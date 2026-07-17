use std::time::Duration;

use tymux_e2e::harness::CliHarness;
use tymux_e2e::{daemon, workspace_bin};
use tymux_proto::v1::CreateSessionRequest;

/// Regression test for a real gap found via v1.0.0-alpha.7's manual
/// release verification: `requirements.md`'s Success Metric ("a status
/// bar renders current session/window state") wasn't actually met —
/// `DisplayMode::Normal` unconditionally rendered the empty string. Fixed
/// in `crates/tymux-cli/src/status_bar.rs`.
#[tokio::test]
async fn status_bar_should_show_session_name_in_normal_mode() {
    let tymuxd_bin = workspace_bin("tymuxd");
    let tymux_bin = workspace_bin("tymux");
    let d = daemon::spawn(&tymuxd_bin);
    let mut client = d.wait_ready().await;
    client
        .create_session(CreateSessionRequest {
            name: "statusbar-e2e".into(),
            command: "/bin/sh".into(),
        })
        .await
        .unwrap();

    let h = CliHarness::spawn(
        &tymux_bin,
        &[
            "--addr",
            &format!("http://{}", d.addr),
            "attach",
            "statusbar-e2e:0.0",
        ],
        &[],
        24,
        80,
    )
    .unwrap();

    assert!(
        h.wait_for("[statusbar-e2e]", Duration::from_secs(3)),
        "normal-mode status bar should show the current session name"
    );

    h.detach(Duration::from_secs(3));
}

#[tokio::test]
async fn status_bar_should_show_prefix_hint_line_when_leader_armed() {
    let tymuxd_bin = workspace_bin("tymuxd");
    let tymux_bin = workspace_bin("tymux");
    let d = daemon::spawn(&tymuxd_bin);
    let mut client = d.wait_ready().await;
    client
        .create_session(CreateSessionRequest {
            name: "statusbar-prefix-e2e".into(),
            command: "/bin/sh".into(),
        })
        .await
        .unwrap();

    // Wide enough that the full prefix-armed hint line ("-- PREFIX --
    // d:detach  [:copy-mode  %:split-h  ...") doesn't wrap — an 80-column
    // terminal isn't enough for it and wrapping is a separate concern
    // this test isn't about.
    let h = CliHarness::spawn(
        &tymux_bin,
        &[
            "--addr",
            &format!("http://{}", d.addr),
            "attach",
            "statusbar-prefix-e2e:0.0",
        ],
        &[],
        24,
        200,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(500));

    h.send(&[0x02]); // arm the leader (C-b) without a follow-up key
    assert!(
        h.wait_for("PREFIX", Duration::from_secs(3)),
        "arming the prefix should show the mode-reactive hint line"
    );
    let hint = h.screen_text();
    assert!(
        hint.contains("detach"),
        "hint line should list the detach binding"
    );

    h.detach(Duration::from_secs(3));
}

#[tokio::test]
async fn no_status_bar_flag_should_add_zero_chrome_bytes() {
    let tymuxd_bin = workspace_bin("tymuxd");
    let tymux_bin = workspace_bin("tymux");
    let d = daemon::spawn(&tymuxd_bin);
    let mut client = d.wait_ready().await;
    client
        .create_session(CreateSessionRequest {
            name: "nostatusbar-e2e".into(),
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
            "nostatusbar-e2e:0.0",
        ],
        &[],
        24,
        80,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(500));

    h.send_str("printf E2E_NOSTATUSBAR_marker\n");
    assert!(h.wait_for("E2E_NOSTATUSBAR_marker", Duration::from_secs(5)));
    let text = h.screen_text();
    assert!(
        !text.contains("nostatusbar-e2e"),
        "--no-status-bar must never render session-name chrome"
    );

    h.detach(Duration::from_secs(3));
}
