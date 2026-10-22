//! What a pane's process is launched with beyond its program name, and the
//! resolution of that description into a concrete argv and stderr file.
//!
//! A [`Launch`] is stored (and persisted) in its *unresolved* form so a revive
//! re-reads argument files and re-opens the stderr file instead of replaying a
//! stale snapshot of them. No shell is involved anywhere: every argument is
//! handed to `exec` as its own argv entry.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::Path;

/// Linux `MAX_ARG_STRLEN`: the longest single argv string `execve` accepts,
/// counting the terminating NUL. Applied to file-backed arguments on every
/// platform so the daemon's behavior does not depend on the host.
pub const MAX_ARG_STRLEN: usize = 128 * 1024;

/// One argv entry, as requested.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ArgSpec {
    Literal(String),
    /// The whole contents of this absolute path, re-read at every spawn.
    FileContents(String),
}

/// Extra spawn parameters beyond the program name, kept on the pane so a
/// revive can respawn an equivalent process.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Launch {
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Appended after `args`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arg_specs: Vec<ArgSpec>,
    /// Absolute path the child's stderr is appended to instead of the pty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_path: Option<String>,
}

/// A [`Launch`] with files read and opened, ready to hand to `exec`.
#[derive(Debug)]
pub struct PreparedLaunch {
    pub args: Vec<String>,
    pub stderr: Option<File>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchErrorKind {
    /// The request itself is malformed or over a limit.
    Invalid,
    /// A referenced file does not exist.
    NotFound,
    /// The environment is not in a state that allows the launch (permissions,
    /// missing parent directory, ...).
    Precondition,
}

#[derive(Debug)]
pub enum LaunchError {
    RelativePath {
        what: &'static str,
        path: String,
    },
    ArgFileRead {
        index: usize,
        path: String,
        source: std::io::Error,
    },
    ArgTooLarge {
        index: usize,
        path: String,
    },
    ArgNotUtf8 {
        index: usize,
        path: String,
    },
    ArgHasNul {
        index: usize,
        path: String,
    },
    StderrOpen {
        path: String,
        source: std::io::Error,
    },
}

impl LaunchError {
    pub fn kind(&self) -> LaunchErrorKind {
        match self {
            Self::RelativePath { .. }
            | Self::ArgTooLarge { .. }
            | Self::ArgNotUtf8 { .. }
            | Self::ArgHasNul { .. } => LaunchErrorKind::Invalid,
            Self::ArgFileRead { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
                LaunchErrorKind::NotFound
            }
            Self::ArgFileRead { .. } | Self::StderrOpen { .. } => LaunchErrorKind::Precondition,
        }
    }
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RelativePath { what, path } => {
                write!(f, "{what} must be an absolute path, got {path:?}")
            }
            Self::ArgFileRead {
                index,
                path,
                source,
            } => {
                write!(f, "arg_specs[{index}]: cannot read {path:?}: {source}")
            }
            Self::ArgTooLarge { index, path } => write!(
                f,
                "arg_specs[{index}]: contents of {path:?} reach {MAX_ARG_STRLEN} bytes, the limit \
                 for one argument (Linux MAX_ARG_STRLEN, including the trailing NUL)"
            ),
            Self::ArgNotUtf8 { index, path } => {
                write!(
                    f,
                    "arg_specs[{index}]: contents of {path:?} are not valid UTF-8"
                )
            }
            Self::ArgHasNul { index, path } => {
                write!(
                    f,
                    "arg_specs[{index}]: contents of {path:?} contain a NUL byte"
                )
            }
            Self::StderrOpen { path, source } => {
                write!(f, "cannot open stderr_path {path:?} for append: {source}")
            }
        }
    }
}

impl std::error::Error for LaunchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ArgFileRead { source, .. } | Self::StderrOpen { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn require_absolute(what: &'static str, path: &str) -> Result<(), LaunchError> {
    if Path::new(path).is_absolute() {
        Ok(())
    } else {
        Err(LaunchError::RelativePath {
            what,
            path: path.to_string(),
        })
    }
}

fn read_arg_file(index: usize, path: &str) -> Result<String, LaunchError> {
    require_absolute("file_contents_path", path)?;
    let read_err = |source| LaunchError::ArgFileRead {
        index,
        path: path.to_string(),
        source,
    };
    let file = File::open(path).map_err(read_err)?;
    // Bounded read: a pathological path (a FIFO, /dev/zero) cannot make the
    // daemon buffer more than the limit.
    let mut bytes = Vec::new();
    file.take(MAX_ARG_STRLEN as u64)
        .read_to_end(&mut bytes)
        .map_err(read_err)?;
    if bytes.len() + 1 > MAX_ARG_STRLEN {
        return Err(LaunchError::ArgTooLarge {
            index,
            path: path.to_string(),
        });
    }
    if bytes.contains(&0) {
        return Err(LaunchError::ArgHasNul {
            index,
            path: path.to_string(),
        });
    }
    String::from_utf8(bytes).map_err(|_| LaunchError::ArgNotUtf8 {
        index,
        path: path.to_string(),
    })
}

fn open_stderr(path: &str) -> Result<File, LaunchError> {
    require_absolute("stderr_path", path)?;
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
        .open(path)
        .map_err(|source| LaunchError::StderrOpen {
            path: path.to_string(),
            source,
        })
}

impl Launch {
    /// Resolves argument files and opens the stderr file. Called at every
    /// spawn, including a revive.
    pub fn prepare(&self) -> Result<PreparedLaunch, LaunchError> {
        let mut args = self.args.clone();
        for (index, spec) in self.arg_specs.iter().enumerate() {
            args.push(match spec {
                ArgSpec::Literal(s) => s.clone(),
                ArgSpec::FileContents(path) => read_arg_file(index, path)?,
            });
        }
        let stderr = self.stderr_path.as_deref().map(open_stderr).transpose()?;
        Ok(PreparedLaunch { args, stderr })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tymux-launch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn with_file_arg(path: &Path) -> Launch {
        Launch {
            arg_specs: vec![ArgSpec::FileContents(path.display().to_string())],
            ..Launch::default()
        }
    }

    #[test]
    fn prepare_should_append_specs_after_args_in_order_keeping_file_bytes_verbatim() {
        let file = tmp("prompt");
        std::fs::write(&file, "line one\n  \"quoted\" $HOME `x`\n").unwrap();
        let launch = Launch {
            args: vec!["a b".into()],
            arg_specs: vec![
                ArgSpec::Literal("lit".into()),
                ArgSpec::FileContents(file.display().to_string()),
            ],
            ..Launch::default()
        };
        assert_eq!(
            launch.prepare().unwrap().args,
            ["a b", "lit", "line one\n  \"quoted\" $HOME `x`\n"]
        );
    }

    #[test]
    fn prepare_should_fail_not_found_when_arg_file_is_missing() {
        let err = with_file_arg(&tmp("missing")).prepare().unwrap_err();
        assert_eq!(err.kind(), LaunchErrorKind::NotFound);
        assert!(err.to_string().contains("arg_specs[0]"), "{err}");
    }

    #[test]
    fn prepare_should_accept_the_largest_argument_and_reject_one_byte_more() {
        let file = tmp("big");
        std::fs::write(&file, vec![b'x'; MAX_ARG_STRLEN - 1]).unwrap();
        assert_eq!(
            with_file_arg(&file).prepare().unwrap().args[0].len(),
            MAX_ARG_STRLEN - 1
        );
        std::fs::write(&file, vec![b'x'; MAX_ARG_STRLEN]).unwrap();
        let err = with_file_arg(&file).prepare().unwrap_err();
        assert!(matches!(err, LaunchError::ArgTooLarge { .. }), "{err}");
        assert_eq!(err.kind(), LaunchErrorKind::Invalid);
    }

    #[test]
    fn prepare_should_reject_non_utf8_nul_and_relative_paths() {
        let file = tmp("bad");
        std::fs::write(&file, [0xff, 0xfe]).unwrap();
        assert!(matches!(
            with_file_arg(&file).prepare().unwrap_err(),
            LaunchError::ArgNotUtf8 { .. }
        ));
        std::fs::write(&file, b"a\0b").unwrap();
        assert!(matches!(
            with_file_arg(&file).prepare().unwrap_err(),
            LaunchError::ArgHasNul { .. }
        ));
        let relative = Launch {
            arg_specs: vec![ArgSpec::FileContents("rel/path".into())],
            ..Launch::default()
        };
        assert!(matches!(
            relative.prepare().unwrap_err(),
            LaunchError::RelativePath { .. }
        ));
        let relative_stderr = Launch {
            stderr_path: Some("err.log".into()),
            ..Launch::default()
        };
        assert!(matches!(
            relative_stderr.prepare().unwrap_err(),
            LaunchError::RelativePath { .. }
        ));
    }

    #[test]
    fn prepare_should_open_stderr_for_append_without_truncating() {
        let log = tmp("err.log");
        std::fs::write(&log, "old\n").unwrap();
        let launch = Launch {
            stderr_path: Some(log.display().to_string()),
            ..Launch::default()
        };
        let mut file = launch.prepare().unwrap().stderr.unwrap();
        std::io::Write::write_all(&mut file, b"new\n").unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "old\nnew\n");
    }

    #[test]
    fn prepare_should_fail_when_stderr_parent_directory_is_missing() {
        let launch = Launch {
            stderr_path: Some("/nonexistent-tymux-dir/err.log".into()),
            ..Launch::default()
        };
        assert_eq!(
            launch.prepare().unwrap_err().kind(),
            LaunchErrorKind::Precondition
        );
    }

    #[test]
    fn launch_should_deserialize_records_written_before_the_new_fields() {
        let old = r#"{"args":["-c","true"],"env":{"K":"V"}}"#;
        let launch: Launch = serde_json::from_str(old).unwrap();
        assert!(launch.arg_specs.is_empty());
        assert_eq!(launch.stderr_path, None);
    }
}
