//! Guest stdout and stderr, captured as tagged log records.
//!
//! wasmtime writes guest output from its own task, outside the instance's
//! tracing span, so each record carries its app, version and pid itself.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::AsyncWrite;
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};

use crate::pid::Pid;

/// Longest line kept; longer lines are split.
const MAX_LINE: usize = 8 * 1024;

/// Who a record is about.
#[derive(Debug)]
pub(crate) struct Tags {
    pub(crate) app: String,
    pub(crate) version: String,
    pub(crate) pid: Pid,
}

/// A stdout or stderr that turns lines into log records.
#[derive(Debug, Clone)]
pub(crate) struct CapturedOutput {
    tags: Arc<Tags>,
    stream: &'static str,
}

impl CapturedOutput {
    pub(crate) fn new(tags: Arc<Tags>, stream: &'static str) -> Self {
        Self { tags, stream }
    }
}

impl IsTerminal for CapturedOutput {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for CapturedOutput {
    fn async_stream(&self) -> Box<dyn AsyncWrite + Send + Sync> {
        Box::new(LineWriter {
            output: self.clone(),
            buffer: Vec::new(),
        })
    }
}

struct LineWriter {
    output: CapturedOutput,
    buffer: Vec<u8>,
}

impl LineWriter {
    fn emit(&self, line: &[u8]) {
        let text = String::from_utf8_lossy(line);
        let tags = &self.output.tags;
        let pid = tags.pid.to_string();
        // stdout is informational; stderr is where guests report problems.
        if self.output.stream == "stderr" {
            tracing::warn!(target: "paddock::guest", app = %tags.app, version = %tags.version, pid = %pid, stream = "stderr", "{text}");
        } else {
            tracing::info!(target: "paddock::guest", app = %tags.app, version = %tags.version, pid = %pid, stream = "stdout", "{text}");
        }
    }

    fn drain_lines(&mut self) {
        while let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=end).collect();
            self.emit(&line[..line.len() - 1]);
        }
        while self.buffer.len() >= MAX_LINE {
            let chunk: Vec<u8> = self.buffer.drain(..MAX_LINE).collect();
            self.emit(&chunk);
        }
    }
}

impl AsyncWrite for LineWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.buffer.extend_from_slice(buf);
        self.drain_lines();
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.buffer.is_empty() {
            let rest = std::mem::take(&mut self.buffer);
            self.emit(&rest);
        }
        Poll::Ready(Ok(()))
    }
}

impl Drop for LineWriter {
    fn drop(&mut self) {
        if !self.buffer.is_empty() {
            let rest = std::mem::take(&mut self.buffer);
            self.emit(&rest);
        }
    }
}
