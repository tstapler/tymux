use std::time::Duration;

use tymux_e2e::harness::CliHarness;
use tymux_e2e::{daemon, workspace_bin};
use tymux_proto::v1::layout::Node;
use tymux_proto::v1::{
    CapturePaneRequest, CreateSessionRequest, Layout, ListSessionsRequest, Orientation,
    SplitPaneRequest,
};

fn leaf_id(layout: &Layout) -> String {
    match layout.node.as_ref().unwrap() {
        Node::Pane(p) => p.id.clone(),
        _ => panic!("expected a leaf child"),
    }
}

#[tokio::test]
async fn split_panes_should_render_and_accept_input_independently() {
    let tymuxd_bin = workspace_bin("tymuxd");
    let tymux_bin = workspace_bin("tymux");
    let d = daemon::spawn(&tymuxd_bin);
    let mut client = d.wait_ready().await;

    let session = client
        .create_session(CreateSessionRequest {
            name: "splits-e2e".into(),
            command: "/bin/sh".into(),
        })
        .await
        .unwrap()
        .into_inner();
    let root_pane = leaf_id(session.windows[0].layout.as_ref().unwrap());

    client
        .split_pane(SplitPaneRequest {
            pane_id: root_pane,
            orientation: Orientation::Vertical as i32,
            command: "/bin/sh".into(),
        })
        .await
        .unwrap();

    let list = client
        .list_sessions(ListSessionsRequest {})
        .await
        .unwrap()
        .into_inner();
    let session = list
        .sessions
        .iter()
        .find(|s| s.name == "splits-e2e")
        .unwrap();
    let (pane0, pane1) = match session.windows[0]
        .layout
        .as_ref()
        .unwrap()
        .node
        .as_ref()
        .unwrap()
    {
        Node::Split(s) => (
            leaf_id(s.children[0].layout.as_ref().unwrap()),
            leaf_id(s.children[1].layout.as_ref().unwrap()),
        ),
        _ => panic!("expected split layout"),
    };

    // `attach` only streams *new* output going forward — it doesn't
    // replay a pane's already-drawn screen — so there's no reliable
    // "prompt just appeared" signal to wait on; a short settle delay
    // after spawn is enough for the raw-mode/attach handshake to finish
    // before input is forwarded.
    let h0 = CliHarness::spawn(
        &tymux_bin,
        &[
            "--addr",
            &format!("http://{}", d.addr),
            "attach",
            "splits-e2e:0.0",
        ],
        &[],
        24,
        80,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    h0.send_str("printf LEFT_ONLY_marker\n");
    assert!(h0.wait_for("LEFT_ONLY_marker", Duration::from_secs(5)));
    assert!(h0.detach(Duration::from_secs(3)));

    let h1 = CliHarness::spawn(
        &tymux_bin,
        &[
            "--addr",
            &format!("http://{}", d.addr),
            "attach",
            "splits-e2e:0.1",
        ],
        &[],
        24,
        80,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    h1.send_str("printf RIGHT_ONLY_marker\n");
    assert!(h1.wait_for("RIGHT_ONLY_marker", Duration::from_secs(5)));
    assert!(h1.detach(Duration::from_secs(3)));

    // Independently confirm isolation via CapturePane on each pane.
    let snap0 = client
        .capture_pane(CapturePaneRequest {
            pane_id: pane0,
            scrollback_offset: 0,
        })
        .await
        .unwrap()
        .into_inner();
    let text0: String = snap0
        .grid
        .iter()
        .flat_map(|r| r.cells.iter())
        .map(|c| c.text.as_str())
        .collect();
    assert!(text0.contains("LEFT_ONLY_marker"));
    assert!(!text0.contains("RIGHT_ONLY_marker"));

    let snap1 = client
        .capture_pane(CapturePaneRequest {
            pane_id: pane1,
            scrollback_offset: 0,
        })
        .await
        .unwrap()
        .into_inner();
    let text1: String = snap1
        .grid
        .iter()
        .flat_map(|r| r.cells.iter())
        .map(|c| c.text.as_str())
        .collect();
    assert!(text1.contains("RIGHT_ONLY_marker"));
    assert!(!text1.contains("LEFT_ONLY_marker"));
}
