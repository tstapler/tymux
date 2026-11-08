//! Manual load probe (ignored by default): input-echo and CapturePane latency of one
//! interactive pane while N chatty panes run, with an optional resize storm.
//! `LOAD_BIN=/path/to/tymuxd LOAD_NOISY=0,10,40 cargo test -p tymux-e2e --release --test load_latency_e2e -- --ignored --nocapture`
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tymux_e2e::{daemon, workspace_bin};
use tymux_proto::v1::tymux_service_client::TymuxServiceClient;
use tymux_proto::v1::{
    attach_event, attach_request, AttachRequest, CapturePaneRequest, CreateSessionRequest,
    GetServerInfoRequest, ListSessionsRequest, PaneActivity, Resize, SetPaneActivityRequest,
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

/// Opens a fresh Attach stream and returns (call returned, first event received) in ms: the
/// priming snapshot cost a reconnecting or newly loaded client waits for.
async fn attach_open_ms(mut client: Client, pane_id: String) -> (f64, f64) {
    let (req_tx, req_rx) = mpsc::channel(8);
    req_tx
        .send(AttachRequest {
            payload: Some(attach_request::Payload::PaneId(pane_id)),
            ..Default::default()
        })
        .await
        .unwrap();
    let start = Instant::now();
    let mut events = client
        .attach(ReceiverStream::new(req_rx))
        .await
        .unwrap()
        .into_inner();
    let returned = start.elapsed().as_secs_f64() * 1000.0;
    let _ = events.message().await;
    (returned, start.elapsed().as_secs_f64() * 1000.0)
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 - 1.0) * p) as usize]
}

/// One latency series, summarised as a single greppable line:
/// `RESULT bin=<label> noisy=<n> storm=<0|1> metric=<name> n=<count> p50=.. p95=.. p99=.. max=..` (ms).
fn report(label: &str, noisy: usize, storm: bool, metric: &str, mut ms: Vec<f64>) {
    println!(
        "RESULT bin={label} noisy={noisy} storm={} metric={metric} n={} p50={:.2} p95={:.2} p99={:.2} max={:.2}",
        u8::from(storm),
        ms.len(),
        pct(&mut ms, 0.5),
        pct(&mut ms, 0.95),
        pct(&mut ms, 0.99),
        pct(&mut ms, 1.0)
    );
}

const NOISY_SCRIPTS: [&str; 3] = [
    "yes",
    "while :; do printf '\\033[31mred\\033[0m \\033[1;32mgreen bold line with some text to fill the row padding padding\\033[0m\\n'; done",
    "while :; do printf '\\033[2J\\033[H'; i=0; while [ $i -lt 30 ]; do printf '\\033[%d;1H\\033[36mredraw row %d\\033[0m\\033[K' $i $i; i=$((i+1)); done; done",
];

/// Everything one probe configuration needs; every knob is pinned by `LOAD_*` env defaults so
/// two runs of the same binary differ only by host noise.
struct Params {
    label: String,
    bin: std::path::PathBuf,
    noisy: usize,
    storm: bool,
    warmup: usize,
    samples: usize,
    captures: usize,
    echo_gap_ms: u64,
    /// Mark noisy panes Background and the probe Interactive (`SetPaneActivity`); daemons without
    /// the RPC ignore it, so the same run is a valid baseline for them.
    activity_hint: bool,
}

/// Spawns a fresh isolated daemon, runs `noisy` chatty panes (round-robin over `NOISY_SCRIPTS`),
/// optionally a resize storm over them, then measures echo and CapturePane latency on a quiet
/// `cat` probe pane.
async fn run_config(p: &Params) {
    let d = daemon::spawn(&p.bin);
    let mut client = d.wait_ready().await;

    let mut noisy_ids = Vec::new();
    let mut _drains = Vec::new();
    for i in 0..p.noisy {
        let id = create(
            &mut client,
            &format!("noisy-{i}"),
            "/bin/sh",
            &["-c", NOISY_SCRIPTS[i % NOISY_SCRIPTS.len()]],
        )
        .await;
        // Standing attach with a draining reader, like the squad's standing streams.
        let (tx, mut rx) = attach(&mut client, &id).await;
        // A hint resets when the pane's last stream ends; sent after attaching it applies at once.
        if p.activity_hint {
            let _ = client
                .set_pane_activity(SetPaneActivityRequest {
                    pane_id: id.clone(),
                    activity: PaneActivity::Background as i32,
                })
                .await;
        }
        _drains.push(tx);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        noisy_ids.push(id);
    }

    let probe = create(&mut client, "probe", "/bin/cat", &[]).await;
    let (probe_tx, mut probe_rx) = attach(&mut client, &probe).await;
    if p.activity_hint {
        let _ = client
            .set_pane_activity(SetPaneActivityRequest {
                pane_id: probe.clone(),
                activity: PaneActivity::Interactive as i32,
            })
            .await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    while probe_rx.try_recv().is_ok() {}

    let storm_handle = if p.storm {
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
    for i in 0..(p.warmup + p.samples) {
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
                if i >= p.warmup {
                    echo.push(at.duration_since(start).as_secs_f64() * 1000.0);
                }
                got = true;
                break;
            }
        }
        if !got && i >= p.warmup {
            lost += 1;
        }
        tokio::time::sleep(Duration::from_millis(p.echo_gap_ms)).await;
    }
    println!(
        "RESULT bin={} noisy={} storm={} metric=echo_lost n={lost}",
        p.label,
        p.noisy,
        u8::from(p.storm)
    );
    if !echo.is_empty() {
        report(&p.label, p.noisy, p.storm, "echo", echo);
    }

    let mut cap_probe = Vec::new();
    let mut cap_noisy = Vec::new();
    for i in 0..p.captures {
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
    report(&p.label, p.noisy, p.storm, "capture_probe", cap_probe);
    if !cap_noisy.is_empty() {
        report(&p.label, p.noisy, p.storm, "capture_noisy", cap_noisy);
    }
    // Attach-open and ListSessions: what squad's AdoptForViewing waits on (3 s budget there).
    let targets: Vec<String> = if noisy_ids.is_empty() {
        vec![probe.clone()]
    } else {
        noisy_ids.clone()
    };
    let (mut open_ret, mut open_first, mut list) = (Vec::new(), Vec::new(), Vec::new());
    for i in 0..30 {
        let (r, f) = attach_open_ms(client.clone(), targets[i % targets.len()].clone()).await;
        open_ret.push(r);
        open_first.push(f);
        let t = Instant::now();
        let _ = client.list_sessions(ListSessionsRequest {}).await;
        list.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    report(&p.label, p.noisy, p.storm, "attach_open_returned", open_ret);
    report(
        &p.label,
        p.noisy,
        p.storm,
        "attach_open_first_event",
        open_first,
    );
    report(&p.label, p.noisy, p.storm, "list_sessions", list);
    // A page reload opens ten at once.
    let mut burst = Vec::new();
    for round in 0..5 {
        let opens: Vec<_> = (0..10)
            .map(|k| {
                tokio::spawn(attach_open_ms(
                    client.clone(),
                    targets[(round * 10 + k) % targets.len()].clone(),
                ))
            })
            .collect();
        for open in opens {
            burst.push(open.await.unwrap().1);
        }
    }
    report(
        &p.label,
        p.noisy,
        p.storm,
        "attach_open_x10_first_event",
        burst,
    );
    // Daemon-reported pool and slow-poll counters; absent on daemons that predate them.
    if let Ok(info) = client
        .get_server_info(GetServerInfoRequest::default())
        .await
    {
        let info = info.into_inner();
        for slow in &info.slow_polls {
            println!(
                "RESULT bin={} noisy={} storm={} metric=slow_polls task={} polls={} slow={} max_poll_ms={:.2}",
                p.label,
                p.noisy,
                u8::from(p.storm),
                slow.task_class,
                slow.polls,
                slow.slow_polls,
                slow.max_poll_micros as f64 / 1000.0
            );
        }
        for pool in &info.work_pools {
            println!(
                "RESULT bin={} noisy={} storm={} metric=pool class={} submitted={} skipped={} rejected={} wait_max_ms={:.2} run_max_ms={:.2}",
                p.label,
                p.noisy,
                u8::from(p.storm),
                pool.class,
                pool.submitted,
                pool.skipped_cancelled,
                pool.rejected,
                pool.wait_micros_max as f64 / 1000.0,
                pool.run_micros_max as f64 / 1000.0
            );
        }
    }
    if let Some(h) = storm_handle {
        h.abort();
    }
}

/// Defaults: noisy panes `0,10,40`, no storm, 20 warmup + 200 echo samples at a 20 ms gap, 60
/// captures. Knobs: `LOAD_BIN` (tymuxd to measure; default the workspace build), `LOAD_LABEL`,
/// `LOAD_NOISY` (comma list), `LOAD_STORM=1`, `LOAD_ACTIVITY=1` (activity hints), `LOAD_WARMUP`, `LOAD_SAMPLES`, `LOAD_CAPTURES`,
/// `LOAD_ECHO_GAP_MS`. Compare daemons with `scripts/load-ab.sh`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual load probe; see scripts/load-ab.sh"]
async fn echo_and_capture_latency_under_load() {
    let bin = std::env::var_os("LOAD_BIN")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| workspace_bin("tymuxd"));
    let label = std::env::var("LOAD_LABEL").unwrap_or_else(|_| "workspace".into());
    let noisy_list: Vec<usize> = std::env::var("LOAD_NOISY")
        .unwrap_or_else(|_| "0,10,40".into())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    for noisy in noisy_list {
        run_config(&Params {
            label: label.clone(),
            bin: bin.clone(),
            noisy,
            storm: env_usize("LOAD_STORM", 0) > 0,
            warmup: env_usize("LOAD_WARMUP", 20),
            samples: env_usize("LOAD_SAMPLES", 200),
            captures: env_usize("LOAD_CAPTURES", 60),
            echo_gap_ms: env_usize("LOAD_ECHO_GAP_MS", 20) as u64,
            activity_hint: env_usize("LOAD_ACTIVITY", 0) > 0,
        })
        .await;
    }
}
