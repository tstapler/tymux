//! Unix child spawn onto a pty slave, replacing `portable_pty`'s
//! `SlavePty::spawn_command`.
//!
//! Why not the library's: its post-fork child walks `/dev/fd` and closes every
//! fd one at a time (and allocates doing it), so each spawn costs O(open fds
//! in the daemon) and risks deadlocking on a malloc lock held by another
//! thread at fork time. This `pre_exec` only makes async-signal-safe calls and
//! closes inherited fds in constant time. Env, cwd and PATH-lookup semantics
//! mirror `CommandBuilder::as_command` so callers see no behavior change.

use std::ffi::{CStr, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::pane::Launch;

/// Spawns `program` with the pty slave behind `master_fd` as stdin/out/err and
/// controlling terminal, in a new session.
pub(crate) fn spawn_on_pty(
    master_fd: RawFd,
    program: &str,
    launch: &Launch,
    cwd: Option<&str>,
) -> Result<std::process::Child> {
    let slave = open_slave(master_fd).context("failed to open pty slave")?;

    // Same base env as CommandBuilder: inherit everything, force SHELL to a
    // usable shell, never leak the daemon's own bearer secret into a pane.
    let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(k, _)| k != "TYMUXD_TOKEN" && k != "SHELL")
        .collect();
    env.push((
        "SHELL".into(),
        portable_pty::CommandBuilder::new(program)
            .get_shell()
            .into(),
    ));
    for (key, value) in &launch.env {
        env.retain(|(k, _)| k != OsStr::new(key));
        env.push((key.into(), value.into()));
    }
    let env_get = |name: &str| {
        env.iter()
            .find(|(k, _)| k == OsStr::new(name))
            .map(|(_, v)| v.clone())
    };

    let home = env_get("HOME")
        .map(PathBuf::from)
        .or_else(passwd_home)
        .unwrap_or_else(|| PathBuf::from("/"));
    let dir = cwd
        .map(PathBuf::from)
        .filter(|d| d.is_dir())
        .unwrap_or(home);

    let resolved = search_path(program.as_ref(), &dir, env_get("PATH").as_deref())?;
    let mut cmd = Command::new(resolved);
    cmd.arg0(program);
    cmd.args(&launch.args);
    cmd.current_dir(&dir);
    cmd.env_clear();
    cmd.envs(env);
    cmd.stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));

    // SAFETY: the closure calls only async-signal-safe libc functions and
    // allocates nothing.
    unsafe {
        cmd.pre_exec(|| {
            for signo in [
                libc::SIGCHLD,
                libc::SIGHUP,
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGTERM,
                libc::SIGALRM,
            ] {
                libc::signal(signo, libc::SIG_DFL);
            }
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            // Fd 0 is the slave by now; without this SIGWINCH never reaches
            // the child.
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            close_inherited_fds();
            Ok(())
        });
    }

    let mut child = cmd.spawn()?;
    child.stdin.take();
    child.stdout.take();
    child.stderr.take();
    Ok(child)
}

/// Closes fds >= 3 leaked in by foreign code. Rust opens everything
/// close-on-exec, so this is belt-and-braces and must stay O(1).
fn close_inherited_fds() {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: plain syscall; on failure (kernel < 5.9) we simply skip it.
    unsafe {
        libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32);
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    for fd in 3..256 {
        // SAFETY: closing an fd we don't own in a just-forked child.
        unsafe {
            libc::close(fd);
        }
    }
}

fn open_slave(master_fd: RawFd) -> io::Result<File> {
    let path = slave_path(master_fd)?;
    // O_NOCTTY: becoming the controlling terminal is TIOCSCTTY's job, in the
    // child, after setsid.
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(path)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn slave_path(master_fd: RawFd) -> io::Result<PathBuf> {
    let mut buf = [0 as libc::c_char; 128];
    // SAFETY: buf is valid for buf.len() bytes; ptsname_r NUL-terminates.
    if unsafe { libc::ptsname_r(master_fd, buf.as_mut_ptr(), buf.len()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: NUL-terminated by the successful call above.
    let name = unsafe { CStr::from_ptr(buf.as_ptr()) };
    Ok(PathBuf::from(OsStr::from_bytes(name.to_bytes())))
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn slave_path(master_fd: RawFd) -> io::Result<PathBuf> {
    let mut buf = [0 as libc::c_char; 128];
    // SAFETY: TIOCPTYGNAME writes a NUL-terminated name of at most 128 bytes.
    if unsafe { libc::ioctl(master_fd, libc::TIOCPTYGNAME as _, buf.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: NUL-terminated by the successful ioctl above.
    let name = unsafe { CStr::from_ptr(buf.as_ptr()) };
    Ok(PathBuf::from(OsStr::from_bytes(name.to_bytes())))
}

fn passwd_home() -> Option<PathBuf> {
    // SAFETY: getpwuid returns null or a pointer valid until the next passwd
    // call; we copy out immediately.
    let ent = unsafe { libc::getpwuid(libc::getuid()) };
    if ent.is_null() {
        return None;
    }
    let home = unsafe { CStr::from_ptr((*ent).pw_dir) };
    Some(PathBuf::from(OsStr::from_bytes(home.to_bytes())))
}

/// Mirrors `CommandBuilder::search_path`: a relative program is tried against
/// the cwd, then each PATH entry; an absolute one must be executable.
fn search_path(exe: &Path, cwd: &Path, path_env: Option<&OsStr>) -> Result<OsString> {
    let executable = |p: &Path| {
        use std::os::unix::fs::PermissionsExt;
        p.metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };
    if exe.is_relative() {
        let abs = cwd.join(exe);
        if abs.exists() {
            return Ok(abs.into_os_string());
        }
        if let Some(path) = path_env {
            for dir in std::env::split_paths(path) {
                let candidate = dir.join(exe);
                if executable(&candidate) {
                    return Ok(candidate.into_os_string());
                }
            }
        }
        bail!(
            "Unable to spawn {} because it doesn't exist on the filesystem and was not found in PATH",
            exe.display()
        );
    }
    if !executable(exe) {
        bail!(
            "Unable to spawn {} because it doesn't exist on the filesystem or is not executable",
            exe.display()
        );
    }
    Ok(exe.as_os_str().to_owned())
}
