//! Manual load probe (ignored by default): input-echo and CapturePane latency of one
//! interactive pane while N chatty panes run, with an optional resize storm.
//! `LOAD_NOISY=40 LOAD_STORM=1 cargo test -p tymux-e2e --release --test load_latency_e2e -- --ignored --nocapture`
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tymux_e2e::{daemon, workspace_bin};
use tymux_proto::v1::tymux_service_client::TymuxServiceClient;
use tymux_proto::v1::{
    attach_event, attach_request, AttachRequest, CapturePaneRequest, CreateSessionRequest, Resize,
};

type Client = TymuxServiceClient<Channel>;

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

async fn create(client: &mut Client, name: &str, command: &str, args: &[&str]) -> String {
    let session = client
        .create_session(CreateSessionRequest {
            name: name.into(),
            command: command.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    match session.windows[0]
        .layout
        .as_ref()
        .unwrap()
        .node
        .as_ref()
        .unwrap()
    {
        tymux_proto::v1::layout::Node::Pane(p) => p.id.clone(),
        _ => panic!("expected leaf"),
    }
}

/// Opens an Attach stream; returns the input sender and a receiver of raw output bytes.
async fn attach(
    client: &mut Client,
    pane_id: &str,
) -> (
    mpsc::Sender<AttachRequest>,
    mpsc::UnboundedReceiver<(Instant, Vec<u8>)>,
) {
    let (req_tx, req_rx) = mpsc::channel(256);
    req_tx
        .send(AttachRequest {
            payload: Some(attach_request::Payload::PaneId(pane_id.into())),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut events = client
        .attach(ReceiverStream::new(req_rx))
        .await
        .unwrap()
        .into_inner();
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok(Some(ev)) = events.message().await {
            match ev.payload {
                Some(attach_event::Payload::Output(b)) => {
                    let _ = out_tx.send((Instant::now(), b));
                }
                Some(attach_event::Payload::OutputChunk(c)) => {
                    let _ = out_tx.send((Instant::now(), c.data));
                }
                _ => {}
            }
        }
    });
    (req_tx, out_rx)
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 - 1.0) * p) as usize]
}

fn report(label: &str, mut ms: Vec<f64>) {
    let n = ms.len();
    println!(
        "RESULT {label}: n={n} p50={:.1}ms p95={:.1}ms p99={:.1}ms max={:.1}ms",
        pct(&mut ms, 0.5),
        pct(&mut ms, 0.95),
        pct(&mut ms, 0.99),
        pct(&mut ms, 1.0)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual load probe; set LOAD_NOISY / LOAD_STORM"]
async fn echo_and_capture_latency_under_load() {
    let noisy = env_usize("LOAD_NOISY", 0);
    let storm = env_usize("LOAD_STORM", 0) > 0;
    let samples = env_usize("LOAD_SAMPLES", 150);
    let d = daemon::spawn(&workspace_bin("tymuxd"));
    let mut client = d.wait_ready().await;

    let mut noisy_ids = Vec::new();
    let mut _drains = Vec::new();
    for i in 0..noisy {
        let script = match i % 3 {
            0 => "yes",
            1 => "while :; do printf '\\033[31mred\\033[0m \\033[1;32mgreen bold line with some text to fill the row padding padding\\033[0m\\n'; done",
            _ => "while :; do printf '\\033[2J\\033[H'; i=0; while [ $i -lt 30 ]; do printf '\\033[%d;1H\\033[36mredraw row %d\\033[0m\\033[K' $i $i; i=$((i+1)); done; done",
        };
        let id = create(
            &mut client,
            &format!("noisy-{i}"),
            "/bin/sh",
            &["-c", script],
        )
        .await;
        // Standing attach with a draining reader, like the squad's standing streams.
        let (tx, mut rx) = attach(&mut client, &id).await;
        _drains.push(tx);
        _drains.push({
            let (t2, _r2) = mpsc::channel(1);
            tokio::spawn(async move { while rx.recv().await.is_some() {} });
            t2
        });
        noisy_ids.push(id);
    }

    let probe = create(&mut client, "probe", "/bin/cat", &[]).await;
    let (probe_tx, mut probe_rx) = attach(&mut client, &probe).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    while probe_rx.try_recv().is_ok() {}

    let storm_handle = if storm {
        let mut c = client.clone();
        let ids = noisy_ids.clone();
        Some(tokio::spawn(async move {
            let mut flip = false;
            let mut holders = Vec::new();
            for id in &ids {
                holders.push(attach(&mut c, id).await.0);
            }
            loop {
                flip = !flip;
                let (rows, cols) = if flip { (35, 120) } else { (22, 80) };
                for h in &holders {
                    let _ = h
                        .send(AttachRequest {
                            payload: Some(attach_request::Payload::Resize(Resize { rows, cols })),
                            ..Default::default()
                        })
                        .await;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }))
    } else {
        None
    };

    let mut echo = Vec::new();
    let mut lost = 0;
    for i in 0..samples {
        let ch = b'a' + (i % 26) as u8;
        let start = Instant::now();
        probe_tx
            .send(AttachRequest {
                payload: Some(attach_request::Payload::Input(vec![ch])),
                ..Default::default()
            })
            .await
            .unwrap();
        let deadline = start + Duration::from_secs(5);
        let mut got = false;
        while let Ok(Some((at, bytes))) =
            tokio::time::timeout_at(deadline.into(), probe_rx.recv()).await
        {
            if bytes.contains(&ch) {
                echo.push(at.duration_since(start).as_secs_f64() * 1000.0);
                got = true;
                break;
            }
        }
        if !got {
            lost += 1;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    println!("RESULT noisy={noisy} storm={storm} echo_lost={lost}");
    if !echo.is_empty() {
        report("echo", echo);
    }

    let mut cap_probe = Vec::new();
    let mut cap_noisy = Vec::new();
    for i in 0..60 {
        let t = Instant::now();
        let _ = client
            .capture_pane(CapturePaneRequest {
                pane_id: probe.clone(),
                scrollback_offset: 0,
            })
            .await;
        cap_probe.push(t.elapsed().as_secs_f64() * 1000.0);
        if !noisy_ids.is_empty() {
            let t = Instant::now();
            let _ = client
                .capture_pane(CapturePaneRequest {
                    pane_id: noisy_ids[i % noisy_ids.len()].clone(),
                    scrollback_offset: 0,
                })
                .await;
            cap_noisy.push(t.elapsed().as_secs_f64() * 1000.0);
        }
    }
    report("capture_probe", cap_probe);
    if !cap_noisy.is_empty() {
        report("capture_noisy", cap_noisy);
    }
    if let Some(h) = storm_handle {
        h.abort();
    }
}
