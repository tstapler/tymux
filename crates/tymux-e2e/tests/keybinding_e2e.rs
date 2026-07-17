use std::time::Duration;

use tymux_e2e::harness::CliHarness;
use tymux_e2e::{daemon, workspace_bin};
use tymux_proto::v1::CreateSessionRequest;

/// Regression test for a real bug found via v1.0.0-alpha.7's manual
/// release verification: a config.toml override like `detach = "C-a d"`
/// (leader other than the global default) was silently unreachable —
/// fixed in `crates/tymux-cli/src/input.rs`.
#[tokio::test]
async fn per_action_keybinding_with_different_leader_should_fire() {
    let tymuxd_bin = workspace_bin("tymuxd");
    let tymux_bin = workspace_bin("tymux");
    let d = daemon::spawn(&tymuxd_bin);
    let mut client = d.wait_ready().await;

    client
        .create_session(CreateSessionRequest {
            name: "keybind-e2e".into(),
            command: "/bin/sh".into(),
        })
        .await
        .unwrap();

    let config_home = std::env::temp_dir().join(format!("tymux-e2e-config-{}", std::process::id()));
    std::fs::create_dir_all(config_home.join("tymux")).unwrap();
    std::fs::write(
        config_home.join("tymux/config.toml"),
        "[keybindings]\ndetach = \"C-a d\"\n",
    )
    .unwrap();

    let h = CliHarness::spawn(
        &tymux_bin,
        &[
            "--addr",
            &format!("http://{}", d.addr),
            "attach",
            "keybind-e2e:0.0",
        ],
        &[("XDG_CONFIG_HOME", config_home.to_str().unwrap())],
        24,
        80,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(500));

    h.send(&[0x01, b'd']); // C-a d — the configured override, not the default C-b d
    assert!(
        h.wait_for("detached", Duration::from_secs(3)),
        "a per-action binding using a different leader than the global default must fire"
    );

    std::fs::remove_dir_all(&config_home).ok();
}

/// The other half of the same regression: a binding matching only by its
/// second key, regardless of which key armed it, would let an unrelated
/// leader's follow-up wrongly fire this binding. Confirms the global
/// leader (C-b) does NOT fire a binding whose configured leader is C-a.
#[tokio::test]
async fn same_second_key_different_leader_should_not_fire() {
    let tymuxd_bin = workspace_bin("tymuxd");
    let tymux_bin = workspace_bin("tymux");
    let d = daemon::spawn(&tymuxd_bin);
    let mut client = d.wait_ready().await;

    client
        .create_session(CreateSessionRequest {
            name: "keybind-control-e2e".into(),
            command: "/bin/sh".into(),
        })
        .await
        .unwrap();

    let config_home =
        std::env::temp_dir().join(format!("tymux-e2e-config-control-{}", std::process::id()));
    std::fs::create_dir_all(config_home.join("tymux")).unwrap();
    std::fs::write(
        config_home.join("tymux/config.toml"),
        "[keybindings]\ndetach = \"C-a d\"\n",
    )
    .unwrap();

    let h = CliHarness::spawn(
        &tymux_bin,
        &[
            "--addr",
            &format!("http://{}", d.addr),
            "attach",
            "keybind-control-e2e:0.0",
        ],
        &[("XDG_CONFIG_HOME", config_home.to_str().unwrap())],
        24,
        80,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(500));

    h.send(&[0x02, b'd']); // C-b d — global leader, but detach is bound to C-a d here
    assert!(
        !h.wait_for("detached", Duration::from_secs(2)),
        "C-b d must not fire a binding whose configured leader is C-a"
    );

    // Clean detach for a tidy teardown (uses the binding that's actually
    // configured here).
    h.send(&[0x01, b'd']);
    h.wait_for("detached", Duration::from_secs(3));

    std::fs::remove_dir_all(&config_home).ok();
}
