//! The process adapter: an extension is a declared executable, nothing else.
//!
//! No code upload, no introspection pass, no code store: the spec is an argv
//! (plus an env), and the guest declares its own schema at load
//! ([`crate::session::Session::handshake`]) or answers the pull
//! (`interface_schema`). That is what makes this shape cheap — and why a host
//! cannot know anything about an extension before it runs, by design.
//!
//! Lifetime discipline (the orphan-leak lesson, carried in from mudra): the
//! child carries `PDEATHSIG` set to SIGTERM in `pre_exec`, so a host that dies
//! takes its extensions with it even on `SIGKILL`; `kill_on_drop` covers the
//! ordinary path where the runtime is still alive to reap.
//!
//! Transport note: this adapter is ONE implementation of the byte channel. A
//! host that reaches its guest another way (a WebSocket-backed duplex, an
//! in-memory pair in tests) drives the same [`crate::session`] types — the
//! session codecs never learn where the bytes come from.

use std::io;
use std::os::unix::process::CommandExt as _;
use std::process::Stdio;

use induction_protocol::Encoding;
use tokio::process::{Child as TokioChild, ChildStdin, ChildStdout, Command as TokioCommand};

use crate::session::Session;

/// A declared extension: what to run, with what environment, speaking which
/// codec. `name` is the declaration key (the guest echoes it in its hello).
#[derive(Debug, Clone)]
pub struct Spec {
    pub name: String,
    /// argv — `argv[0]` is the executable. Whitespace is never re-split: a spec
    /// is a vector of arguments, and the host does not guess the author's arity.
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub encoding: Encoding,
}

impl Spec {
    pub fn new(name: impl Into<String>, argv: impl IntoIterator<Item = String>) -> Self {
        Self {
            name: name.into(),
            argv: argv.into_iter().collect(),
            env: Vec::new(),
            encoding: Encoding::default(),
        }
    }

    /// Convenience for a plain path, the common case.
    pub fn executable(name: impl Into<String>, path: impl Into<String>) -> Self {
        Self::new(name, [path.into()])
    }

    pub fn with_encoding(mut self, encoding: Encoding) -> Self {
        self.encoding = encoding;
        self
    }

    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// The command line, for logs and diagnostics.
    pub fn command_line(&self) -> String {
        self.argv.join(" ")
    }
}

/// A spawned extension: the process handle plus its session halves.
pub struct Process {
    pub name: String,
    pub child: TokioChild,
    pub session: Session<ChildStdout, ChildStdin>,
}

impl Process {
    /// Send SIGKILL and stop supervising. Idempotent.
    pub fn kill(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Spawn a declared extension. `PDEATHSIG` rides `pre_exec`; `kill_on_drop`
/// covers the ordinary teardown; stderr is inherited so a guest's diagnostics
/// reach the host's log.
///
/// `#[allow(unsafe_code)]`: the closure is async-signal-safe `prctl` only. It
/// is the one unsafe site in this workspace (the workspace lint is `deny`, so
/// an unconditional allow cannot hide anywhere else).
#[allow(unsafe_code)]
pub fn spawn(spec: &Spec) -> io::Result<Process> {
    let (program, args) = spec
        .argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty extension argv"))?;

    let mut std_cmd = std::process::Command::new(program);
    std_cmd.args(args);
    for (k, v) in &spec.env {
        std_cmd.env(k, v);
    }
    // The codec declaration crosses as an env, never as an argv: a fifo-shaped
    // spec passes the author's own arguments, and an appended argv would break
    // the author's arity.
    std_cmd.env(induction_protocol::ENCODING_ENV, spec.encoding.as_str());
    unsafe {
        std_cmd.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut cmd = TokioCommand::from(std_cmd);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = cmd.spawn()?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("extension spawn: no stdin pipe"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("extension spawn: no stdout pipe"))?;

    Ok(Process {
        name: spec.name.clone(),
        child,
        session: Session::new(stdout, stdin),
    })
}