//! Diagnostics for the daemon's listening sockets and file-descriptor budget:
//! accept-error logging that tonic suppresses, the state of a listener fd for
//! probe-failure logs, and `RLIMIT_NOFILE` handling.

use std::io;
use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use futures::{Stream, TryStreamExt};

/// Soft-limit ceiling on macOS, where `setrlimit` fails above `OPEN_MAX`
/// even though the reported hard limit is `RLIM_INFINITY`.
const MACOS_OPEN_MAX: u64 = 10_240;
/// Ceiling elsewhere, so an unlimited hard limit does not mean a million fds.
const DEFAULT_NOFILE_CAP: u64 = 1_048_576;
/// Warn from the probe loop once this share of the fd limit is in use.
const FD_PRESSURE_PERCENT: u64 = 80;
/// At most one accept-error log line per listener per this interval.
const ACCEPT_ERROR_LOG_INTERVAL: Duration = Duration::from_secs(10);

/// The soft limit to request: as high as the hard limit and `cap` allow, never lower than now.
pub fn desired_soft_limit(soft: u64, hard: u64, cap: u64) -> u64 {
    soft.max(hard.min(cap))
}

fn nofile_cap() -> u64 {
    if cfg!(target_os = "macos") {
        MACOS_OPEN_MAX
    } else {
        DEFAULT_NOFILE_CAP
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NofileLimits {
    pub soft_before: u64,
    pub soft_after: u64,
    pub hard: u64,
}

/// Raises the soft `RLIMIT_NOFILE` toward the hard limit. Under launchd the soft limit can be
/// as low as 256, which a few dozen panes exhaust.
pub fn raise_nofile_limit() -> io::Result<NofileLimits> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `lim` is a valid, writable rlimit.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let soft_before = lim.rlim_cur as u64;
    let hard = lim.rlim_max as u64;
    let target = desired_soft_limit(soft_before, hard, nofile_cap());
    if target == soft_before {
        return Ok(NofileLimits {
            soft_before,
            soft_after: soft_before,
            hard,
        });
    }
    lim.rlim_cur = target as libc::rlim_t;
    // SAFETY: `lim` is a valid rlimit; rlim_cur <= rlim_max by construction.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(NofileLimits {
        soft_before,
        soft_after: target,
        hard,
    })
}

/// The current soft `RLIMIT_NOFILE`, or 0 if it cannot be read.
pub fn nofile_soft_limit() -> u64 {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `lim` is a valid, writable rlimit.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return 0;
    }
    lim.rlim_cur as u64
}

/// Number of fds this process holds open, or 0 if `/dev/fd` cannot be listed.
pub fn open_fd_count() -> u64 {
    // /dev/fd is a directory on macOS and a symlink to /proc/self/fd on Linux.
    match std::fs::read_dir("/dev/fd") {
        // The directory handle itself shows up in its own listing.
        Ok(dir) => (dir.count() as u64).saturating_sub(1),
        Err(_) => 0,
    }
}

/// True once `open` fds are at or above [`FD_PRESSURE_PERCENT`] of `limit`.
pub fn fd_pressure(open: u64, limit: u64) -> bool {
    limit > 0 && open * 100 >= limit * FD_PRESSURE_PERCENT
}

/// A one-line description of what `fd` is now: closed, open but no longer a socket, or a socket.
/// A stray close shows up as `EBADF`, or as "not a socket" once the number is reused.
pub fn fd_state(fd: RawFd) -> String {
    // SAFETY: F_GETFD takes no pointer and is valid on any integer.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return format!("fcntl(F_GETFD) failed: {}", io::Error::last_os_error());
    }
    // SAFETY: an all-zero sockaddr_storage is a valid out-buffer for getsockname.
    let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: `addr`/`len` describe a writable buffer of exactly that size.
    let rc = unsafe {
        libc::getsockname(
            fd,
            (&mut addr as *mut libc::sockaddr_storage).cast(),
            &mut len,
        )
    };
    if rc < 0 {
        return format!(
            "open (fd_flags={flags:#x}) but not a socket: {}",
            io::Error::last_os_error()
        );
    }
    format!("open socket (fd_flags={flags:#x})")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptErrorKind {
    /// The listener itself is unusable; tonic will spin on this error forever.
    Fatal(&'static str),
    /// Descriptor exhaustion: the listener is fine but cannot accept right now.
    FdExhausted(&'static str),
    Transient,
}

pub fn classify_accept_error(e: &io::Error) -> AcceptErrorKind {
    match e.raw_os_error() {
        Some(n) if n == libc::EBADF => AcceptErrorKind::Fatal("EBADF"),
        Some(n) if n == libc::ENOTSOCK => AcceptErrorKind::Fatal("ENOTSOCK"),
        Some(n) if n == libc::EINVAL => AcceptErrorKind::Fatal("EINVAL"),
        Some(n) if n == libc::EMFILE => AcceptErrorKind::FdExhausted("EMFILE"),
        Some(n) if n == libc::ENFILE => AcceptErrorKind::FdExhausted("ENFILE"),
        _ => AcceptErrorKind::Transient,
    }
}

/// Lets at most one accept error through per interval and counts the ones it swallowed. Time is
/// passed in so the decision is testable without sleeping.
#[derive(Debug, Default)]
pub struct AcceptErrorLimiter {
    last_logged: Option<Instant>,
    suppressed: u64,
}

impl AcceptErrorLimiter {
    /// `Some(suppressed_since_last_log)` if this error should be logged now.
    pub fn admit(&mut self, now: Instant) -> Option<u64> {
        let due = self
            .last_logged
            .is_none_or(|t| now.duration_since(t) >= ACCEPT_ERROR_LOG_INTERVAL);
        if due {
            self.last_logged = Some(now);
            Some(std::mem::take(&mut self.suppressed))
        } else {
            self.suppressed += 1;
            None
        }
    }
}

fn log_accept_error(
    listener: &'static str,
    fd: RawFd,
    e: &io::Error,
    suppressed_since_last_log: u64,
) {
    let errno = e.raw_os_error();
    match classify_accept_error(e) {
        AcceptErrorKind::Fatal(name) => tracing::error!(
            listener, errno, errno_name = name, error = %e, fd_state = %fd_state(fd),
            suppressed_since_last_log,
            "accept failed with a fatal errno: the listener socket is broken and will not recover"
        ),
        AcceptErrorKind::FdExhausted(name) => tracing::error!(
            listener, errno, errno_name = name, error = %e,
            open_fds = open_fd_count(), fd_limit = nofile_soft_limit(),
            suppressed_since_last_log,
            "accept failed: out of file descriptors"
        ),
        AcceptErrorKind::Transient => tracing::warn!(
            listener, errno, error = %e, suppressed_since_last_log, "accept failed"
        ),
    }
}

/// Logs every accept error (rate-limited) and passes the stream through unchanged. tonic 0.12
/// logs these at trace level only, so a dead listener is otherwise silent.
pub fn log_accept_errors<S, T>(
    incoming: S,
    listener: &'static str,
    fd: RawFd,
) -> impl Stream<Item = io::Result<T>>
where
    S: Stream<Item = io::Result<T>>,
{
    let mut limiter = AcceptErrorLimiter::default();
    incoming.inspect_err(move |e| {
        if let Some(suppressed) = limiter.admit(Instant::now()) {
            log_accept_error(listener, fd, e, suppressed);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[test]
    fn desired_soft_limit_should_rise_to_the_hard_limit_when_under_the_cap() {
        assert_eq!(desired_soft_limit(256, 4096, 10_240), 4096);
    }

    #[test]
    fn desired_soft_limit_should_stop_at_the_cap_when_the_hard_limit_is_unlimited() {
        assert_eq!(desired_soft_limit(256, u64::MAX, 10_240), 10_240);
    }

    #[test]
    fn desired_soft_limit_should_never_lower_an_already_higher_soft_limit() {
        assert_eq!(desired_soft_limit(65_536, u64::MAX, 10_240), 65_536);
    }

    #[test]
    fn raise_nofile_limit_should_not_leave_the_soft_limit_below_where_it_started() {
        let limits = raise_nofile_limit().unwrap();
        assert!(limits.soft_after >= limits.soft_before);
        assert_eq!(nofile_soft_limit(), limits.soft_after);
    }

    #[test]
    fn fd_pressure_should_trigger_at_eighty_percent() {
        assert!(!fd_pressure(79, 100));
        assert!(fd_pressure(80, 100));
        assert!(!fd_pressure(5, 0));
    }

    #[test]
    fn open_fd_count_should_include_the_fds_this_test_holds() {
        // Parallel tests open and close fds, so assert a floor, not a delta.
        let held: Vec<_> = (0..50)
            .map(|_| std::fs::File::open("/dev/null").unwrap())
            .collect();
        assert!(open_fd_count() >= held.len() as u64);
    }

    #[test]
    fn classify_accept_error_should_flag_listener_destroying_errnos_as_fatal() {
        for (errno, name) in [
            (libc::EBADF, "EBADF"),
            (libc::ENOTSOCK, "ENOTSOCK"),
            (libc::EINVAL, "EINVAL"),
        ] {
            assert_eq!(
                classify_accept_error(&io::Error::from_raw_os_error(errno)),
                AcceptErrorKind::Fatal(name)
            );
        }
    }

    #[test]
    fn classify_accept_error_should_separate_fd_exhaustion_from_other_errors() {
        assert_eq!(
            classify_accept_error(&io::Error::from_raw_os_error(libc::EMFILE)),
            AcceptErrorKind::FdExhausted("EMFILE")
        );
        assert_eq!(
            classify_accept_error(&io::Error::from_raw_os_error(libc::ECONNABORTED)),
            AcceptErrorKind::Transient
        );
    }

    #[test]
    fn limiter_should_admit_once_per_interval_and_report_what_it_swallowed() {
        let mut limiter = AcceptErrorLimiter::default();
        let t0 = Instant::now();
        assert_eq!(limiter.admit(t0), Some(0));
        assert_eq!(limiter.admit(t0 + Duration::from_secs(1)), None);
        assert_eq!(limiter.admit(t0 + Duration::from_secs(2)), None);
        assert_eq!(
            limiter.admit(t0 + ACCEPT_ERROR_LOG_INTERVAL),
            Some(2),
            "the next admitted error carries the count of suppressed ones"
        );
    }

    #[test]
    fn fd_state_should_report_a_socket_then_a_non_socket_then_a_closed_fd() {
        use std::os::fd::AsRawFd;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let state = fd_state(listener.as_raw_fd());
        assert!(state.starts_with("open socket"), "{state}");

        let file = std::fs::File::open("/dev/null").unwrap();
        assert!(fd_state(file.as_raw_fd()).contains("not a socket"));

        let closed = file.as_raw_fd();
        drop(file);
        assert!(fd_state(closed).contains("fcntl(F_GETFD) failed"));
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn log_accept_errors_should_log_a_fatal_errno_and_still_forward_the_error() {
        let src = futures::stream::iter(vec![
            Err::<u8, _>(io::Error::from_raw_os_error(libc::EBADF)),
            Ok(7),
        ]);
        let out: Vec<_> = log_accept_errors(src, "tcp", -1).collect().await;

        assert!(out[0].is_err() && matches!(out[1], Ok(7)));
        assert!(logs_contain("fatal errno"));
        assert!(logs_contain("EBADF"));
    }
}
