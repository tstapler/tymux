//! Unix child spawn onto a pty slave, replacing `portable_pty`'s
//! `SlavePty::spawn_command`.
//!
//! Why not the library's: its post-fork child walks `/dev/fd` and closes every
//! fd one at a time (and allocates doing it), so each spawn costs O(open fds
//! in the daemon) and risks deadlocking on a malloc lock held by another
//! thread at fork time. This `pre_exec` only makes async-signal-safe calls and
//! closes inherited fds in constant time. Env, cwd and PATH-lookup semantics
//! mirror `CommandBuilder::as_command` so callers see no behavior change.
//!
//! On Linux/glibc the child is started with `posix_spawn` (CLONE_VM|VFORK), so
//! cost no longer grows with the daemon's thread and mapping count the way
//! `fork` does. Elsewhere it falls back to `Command` + `pre_exec`.

use std::ffi::{CStr, OsStr, OsString};
use std::io;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
use std::{
    fs::File,
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    process::{Command, Stdio},
};

use anyhow::{bail, Context, Result};

use crate::pane::Launch;

/// Spawns `program` with the pty slave behind `master_fd` as stdin/out/err and
/// controlling terminal, in a new session.
pub(crate) fn spawn_on_pty(
    master_fd: RawFd,
    program: &str,
    launch: &Launch,
    cwd: Option<&str>,
) -> Result<Box<dyn portable_pty::Child + Send + Sync>> {
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
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        let slave = slave_path(master_fd).context("failed to find pty slave")?;
        let child = posix::spawn(&resolved, program, &launch.args, &env, &dir, &slave)?;
        Ok(Box::new(child))
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        let slave = open_slave(master_fd).context("failed to open pty slave")?;
        Ok(Box::new(spawn_with_command(
            resolved, program, launch, &dir, env, slave,
        )?))
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn spawn_with_command(
    resolved: OsString,
    program: &str,
    launch: &Launch,
    dir: &Path,
    env: Vec<(OsString, OsString)>,
    slave: File,
) -> Result<std::process::Child> {
    let mut cmd = Command::new(resolved);
    cmd.arg0(program);
    cmd.args(&launch.args);
    cmd.current_dir(dir);
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
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
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

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
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

#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod posix {
    use super::*;
    use std::ffi::CString;
    use std::mem::MaybeUninit;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    fn cstr(s: &OsStr) -> Result<CString> {
        CString::new(s.as_bytes()).context("string contains an interior NUL byte")
    }

    /// RAII guards so every early return frees the spawn attr/actions.
    struct Attr(libc::posix_spawnattr_t);
    impl Drop for Attr {
        fn drop(&mut self) {
            unsafe { libc::posix_spawnattr_destroy(&mut self.0) };
        }
    }
    struct Actions(libc::posix_spawn_file_actions_t);
    impl Drop for Actions {
        fn drop(&mut self) {
            unsafe { libc::posix_spawn_file_actions_destroy(&mut self.0) };
        }
    }

    fn check(rc: libc::c_int) -> io::Result<()> {
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(rc))
        }
    }

    pub(super) fn spawn(
        resolved: &OsStr,
        arg0: &str,
        args: &[String],
        env: &[(OsString, OsString)],
        dir: &Path,
        slave: &Path,
    ) -> Result<PosixChild> {
        let path = cstr(resolved)?;
        let mut argv_owned = vec![cstr(OsStr::new(arg0))?];
        for a in args {
            argv_owned.push(cstr(OsStr::new(a))?);
        }
        let mut argv: Vec<*mut libc::c_char> =
            argv_owned.iter().map(|c| c.as_ptr() as *mut _).collect();
        argv.push(std::ptr::null_mut());

        let mut envp_owned = Vec::with_capacity(env.len());
        for (k, v) in env {
            let mut kv = k.as_bytes().to_vec();
            kv.push(b'=');
            kv.extend_from_slice(v.as_bytes());
            envp_owned.push(CString::new(kv).context("env entry contains an interior NUL byte")?);
        }
        let mut envp: Vec<*mut libc::c_char> =
            envp_owned.iter().map(|c| c.as_ptr() as *mut _).collect();
        envp.push(std::ptr::null_mut());

        let dir_c = cstr(dir.as_os_str())?;
        let slave_c = cstr(slave.as_os_str())?;

        // SAFETY: every init/destroy pair is guarded above; pointers passed to
        // posix_spawn outlive the call (owned CStrings in this frame).
        unsafe {
            let mut attr = Attr(MaybeUninit::zeroed().assume_init());
            check(libc::posix_spawnattr_init(&mut attr.0))?;
            let mut defaults: libc::sigset_t = MaybeUninit::zeroed().assume_init();
            libc::sigemptyset(&mut defaults);
            for signo in [
                libc::SIGCHLD,
                libc::SIGHUP,
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGTERM,
                libc::SIGALRM,
                libc::SIGPIPE, // Rust sets SIGPIPE to ignore in the daemon
            ] {
                libc::sigaddset(&mut defaults, signo);
            }
            let mut mask: libc::sigset_t = MaybeUninit::zeroed().assume_init();
            libc::sigemptyset(&mut mask);
            check(libc::posix_spawnattr_setsigdefault(&mut attr.0, &defaults))?;
            check(libc::posix_spawnattr_setsigmask(&mut attr.0, &mask))?;
            check(libc::posix_spawnattr_setflags(
                &mut attr.0,
                (libc::POSIX_SPAWN_SETSID
                    | libc::POSIX_SPAWN_SETSIGDEF
                    | libc::POSIX_SPAWN_SETSIGMASK) as _,
            ))?;

            let mut fa = Actions(MaybeUninit::zeroed().assume_init());
            check(libc::posix_spawn_file_actions_init(&mut fa.0))?;
            check(libc::posix_spawn_file_actions_addchdir_np(
                &mut fa.0,
                dir_c.as_ptr(),
            ))?;
            // Runs after the child's setsid, and without O_NOCTTY: the first
            // open of a tty by a session leader with no controlling terminal
            // makes it the controlling terminal. posix_spawn has no TIOCSCTTY.
            check(libc::posix_spawn_file_actions_addopen(
                &mut fa.0,
                0,
                slave_c.as_ptr(),
                libc::O_RDWR,
                0,
            ))?;
            check(libc::posix_spawn_file_actions_adddup2(&mut fa.0, 0, 1))?;
            check(libc::posix_spawn_file_actions_adddup2(&mut fa.0, 0, 2))?;
            // Constant-time close of everything inherited above fd 2.
            check(libc::posix_spawn_file_actions_addclosefrom_np(&mut fa.0, 3))?;

            let mut pid: libc::pid_t = 0;
            let rc = libc::posix_spawn(
                &mut pid,
                path.as_ptr(),
                &fa.0,
                &attr.0,
                argv.as_ptr(),
                envp.as_ptr(),
            );
            check(rc).with_context(|| format!("failed to spawn {arg0}"))?;
            Ok(PosixChild { pid })
        }
    }

    /// A reapable child with no `std::process::Child` behind it.
    #[derive(Debug)]
    pub(super) struct PosixChild {
        pid: libc::pid_t,
    }

    fn waitpid(pid: libc::pid_t, flags: libc::c_int) -> io::Result<Option<ExitStatus>> {
        let mut status = 0;
        loop {
            let rc = unsafe { libc::waitpid(pid, &mut status, flags) };
            if rc == pid {
                return Ok(Some(ExitStatus::from_raw(status)));
            }
            if rc == 0 {
                return Ok(None);
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }

    impl portable_pty::Child for PosixChild {
        fn try_wait(&mut self) -> io::Result<Option<portable_pty::ExitStatus>> {
            Ok(waitpid(self.pid, libc::WNOHANG)?.map(Into::into))
        }

        fn wait(&mut self) -> io::Result<portable_pty::ExitStatus> {
            Ok(waitpid(self.pid, 0)?
                .expect("blocking waitpid returned no status")
                .into())
        }

        fn process_id(&self) -> Option<u32> {
            Some(self.pid as u32)
        }
    }

    #[derive(Debug)]
    struct Killer(libc::pid_t);

    impl portable_pty::ChildKiller for Killer {
        fn kill(&mut self) -> io::Result<()> {
            // SIGHUP like portable-pty; Pane::kill owns the SIGKILL escalation.
            if unsafe { libc::kill(self.0, libc::SIGHUP) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
            Box::new(Killer(self.0))
        }
    }

    impl portable_pty::ChildKiller for PosixChild {
        fn kill(&mut self) -> io::Result<()> {
            Killer(self.pid).kill()
        }

        fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
            Box::new(Killer(self.pid))
        }
    }
}
