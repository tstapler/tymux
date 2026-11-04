//! A pane child gets the soft fd limit set via `set_child_nofile_soft_limit`, not the daemon's
//! raised one. Own file because the setting is process-global.

use tymux_core::{Engine, Launch, NullPersistenceBackend};

// rlim_t is u32 on some targets.
#[allow(clippy::unnecessary_cast)]
#[tokio::test]
async fn pane_child_should_start_with_the_configured_soft_fd_limit() {
    const CHILD_LIMIT: u64 = 64;
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `lim` is a valid, writable rlimit.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) }, 0);
    assert!(
        lim.rlim_cur as u64 > CHILD_LIMIT,
        "the test process needs a soft limit above {CHILD_LIMIT} to prove a reduction"
    );
    tymux_core::set_child_nofile_soft_limit(CHILD_LIMIT);

    let out = std::env::temp_dir().join(format!("tymux-nofile-{}", std::process::id()));
    // Linux/glibc spawns with posix_spawn and lowers the limit from the parent afterwards, so
    // the child can win the race and see the raised limit first: wait (bounded) for it to drop.
    let read_limit = if cfg!(all(target_os = "linux", target_env = "gnu")) {
        format!(
            "i=0; while [ \"$(ulimit -n)\" -gt {CHILD_LIMIT} ] && [ $i -lt 200000 ]; do i=$((i+1)); done; ulimit -n"
        )
    } else {
        "ulimit -n".to_string()
    };
    let launch = Launch {
        args: vec!["-c".into(), format!("{read_limit} > {}", out.display())],
        ..Launch::default()
    };
    let engine = Engine::with_persistence(Box::new(NullPersistenceBackend));
    let session = engine
        .create_session_with("nofile".into(), Some("/bin/sh".into()), launch, None)
        .unwrap();
    let id = match &engine.session_snapshot(session).unwrap().windows[0].layout {
        tymux_core::LayoutSnapshot::Leaf(i) => i.id,
        _ => unreachable!(),
    };
    engine.held_pane(id).unwrap().wait_exit().await;

    let seen = std::fs::read_to_string(&out).unwrap();
    std::fs::remove_file(&out).ok();
    assert_eq!(seen.trim(), CHILD_LIMIT.to_string());
}
