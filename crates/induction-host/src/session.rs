//! The session: framed lines in and out, every await budgeted.
//!
//! Read and write are deliberately SEPARATE types ([`Reader`] / [`Writer`]) so a
//! host can hold one borrow of each across a `select!` — the shape mudra's host
//! already needs (its event source and the child's stdout are two live sources).
//! [`Session`] bundles them for hosts that drive strictly request/response.
//!
//! Writes and the handshake always carry a budget; reads come in two forms, and
//! the distinction is load-bearing. [`Reader::next_line`] takes a budget (the
//! handshake, a peer probe); [`Reader::next_frame`] awaits with none, because a
//! live extension may legitimately be silent — an extension with nothing to say
//! produces no frames, and the host's events arrive on the host's own clock. A
//! stalled peer stalls only itself; the host decides what a missed WRITE budget
//! means.

use std::time::Duration;

use induction_protocol::{Hello, initialize_line};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, Lines};

/// The budgets a session runs on. Handshake and write are the two places a
/// peer can hang the host; a read budget covers the idle-read case.
#[derive(Debug, Clone, Copy)]
pub struct Budgets {
    /// One `initialize` → `hello` round trip.
    pub handshake: Duration,
    /// One frame write (write + flush).
    pub write: Duration,
    /// One budgeted read ([`Reader::next_line`]): the handshake, and peer
    /// probes. The driver reads WITHOUT a budget on purpose — see
    /// [`Reader::next_frame`].
    pub read: Duration,
}

impl Default for Budgets {
    fn default() -> Self {
        Self {
            handshake: Duration::from_secs(5),
            write: Duration::from_secs(5),
            read: Duration::from_secs(5),
        }
    }
}

/// The read half: buffered lines with a budget per line.
pub struct Reader<R> {
    lines: Lines<BufReader<R>>,
}

impl<R: AsyncRead + Unpin> Reader<R> {
    pub fn new(read: R) -> Self {
        Self {
            lines: BufReader::new(read).lines(),
        }
    }

    /// The next frame line, or `None` on EOF, on a read error, or when the
    /// budget expires. A timeout is indistinguishable from EOF here on purpose:
    /// both mean "no frame arrived", and the caller's policy (log, stall, end,
    /// respawn) is the same.
    pub async fn next_line(&mut self, budget: Duration) -> Option<String> {
        match tokio::time::timeout(budget, self.lines.next_line()).await {
            Ok(Ok(Some(line))) => Some(line),
            _ => None,
        }
    }

    /// The next frame line, awaiting with NO budget.
    ///
    /// A live session may legitimately be silent: an extension with nothing to
    /// say produces no frames, and the host's events arrive on the host's own
    /// clock. So an idle read must not become an outcome — only the channel
    /// closing does. `None` here means exactly one thing: the child died,
    /// closed its stdout, or the read errored.
    pub async fn next_frame(&mut self) -> Option<String> {
        match self.lines.next_line().await {
            Ok(Some(line)) => Some(line),
            _ => None,
        }
    }

    /// Decode the load-time hello from the next line.
    pub async fn hello(&mut self, budget: Duration) -> Option<Hello> {
        let line = self.next_line(budget).await?;
        Hello::parse_line(&line)
    }
}

/// The write half: one frame per call, flushed, budgeted.
pub struct Writer<W> {
    out: W,
}

impl<W: AsyncWrite + Unpin> Writer<W> {
    pub fn new(write: W) -> Self {
        Self { out: write }
    }

    /// Write one frame. `false` on a budget miss or an io error — the caller
    /// treats both as "this frame did not land" (so a cursor, if the host keeps
    /// one, must NOT be advanced).
    pub async fn write_line(&mut self, line: &str, budget: Duration) -> bool {
        let mut payload = String::with_capacity(line.len() + 1);
        payload.push_str(line);
        payload.push('\n');
        matches!(
            tokio::time::timeout(budget, async {
                self.out.write_all(payload.as_bytes()).await?;
                self.out.flush().await
            })
            .await,
            Ok(Ok(()))
        )
    }
}

/// Read + write halves of one session.
pub struct Session<R, W> {
    pub reader: Reader<R>,
    pub writer: Writer<W>,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> Session<R, W> {
    pub fn new(read: R, write: W) -> Self {
        Self {
            reader: Reader::new(read),
            writer: Writer::new(write),
        }
    }

    pub fn from_halves(reader: Reader<R>, writer: Writer<W>) -> Self {
        Self { reader, writer }
    }

    /// The load-time handshake: `initialize` out, `hello` in, both budgeted.
    ///
    /// `None` on either half failing, or on a hello that does not decode — a
    /// contract mismatch is a refused session, never a defaulted one.
    pub async fn handshake(&mut self, budget: Duration) -> Option<Hello> {
        if !self.writer.write_line(&initialize_line(), budget).await {
            return None;
        }
        self.reader.hello(budget).await
    }
}
