//! The plumbing the built-in ACP agents share (`claude`, `codex`): JSON
//! lines on a pipe, the ACP client on our stdout, and JSON-RPC errors.

use std::{
    collections::HashMap,
    io::{self, BufRead, Write},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Sender},
    },
};

use serde_json::{Value, json};

/// The longest JSON line read from a pipe: the ACP client's requests, and
/// Claude's and Codex's messages.  The largest legitimate ones carry base64
/// images (the API takes an image of up to 5 MB, about 7 MB encoded, and a
/// prompt may hold several); 64 MiB is far above that and still keeps one
/// runaway line from taking all memory.
pub(crate) const LINE_LIMIT_BYTES: usize = 64 * 1024 * 1024;

/// One line read with a limit on its length.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LineRead {
    /// The line, without its `\n` or `\r\n`.
    Line(Vec<u8>),
    /// A line over the limit: read past and dropped, never held whole.
    TooLong { length_bytes: usize },
}

/// Collects one line from a buffered reader's chunks, keeping at most
/// `limit_bytes` of it.  Shared by the blocking reader here and the async
/// one in `transport`, so both split lines the same way.
pub(crate) struct LineAccumulator {
    line: Vec<u8>,
    length_bytes: usize,
    limit_bytes: usize,
    too_long: bool,
    has_newline: bool,
    read_any: bool,
}

impl LineAccumulator {
    pub fn new(limit_bytes: usize) -> Self {
        assert!(
            limit_bytes > 0,
            "a line limit of zero would refuse every line"
        );
        LineAccumulator {
            line: Vec::new(),
            length_bytes: 0,
            limit_bytes,
            too_long: false,
            has_newline: false,
            read_any: false,
        }
    }

    /// Take what belongs to the line from `chunk`.  Returns how many bytes to
    /// consume, and whether the line is complete (its newline was seen).
    pub fn feed(&mut self, chunk: &[u8]) -> (usize, bool) {
        assert!(!self.has_newline, "a complete line takes no more input");
        let (body, has_newline) = match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => (&chunk[..i], true),
            None => (chunk, false),
        };
        self.read_any |= !chunk.is_empty();
        self.length_bytes = self.length_bytes.saturating_add(body.len());
        if !self.too_long && self.length_bytes > self.limit_bytes {
            self.too_long = true;
            self.line = Vec::new();
        }
        if !self.too_long {
            self.line.extend_from_slice(body);
        }
        self.has_newline = has_newline;
        debug_assert!(self.line.len() <= self.limit_bytes);
        (body.len() + usize::from(has_newline), has_newline)
    }

    /// The line, once its newline was seen or the input ended.  `None` at
    /// the end of input with nothing read, as `BufRead::lines` ends.
    pub fn finish(mut self) -> Option<LineRead> {
        if !self.read_any {
            return None;
        }
        if self.too_long {
            return Some(LineRead::TooLong {
                length_bytes: self.length_bytes,
            });
        }
        // Like `BufRead::lines`: a `\r` goes with the newline it precedes.
        if self.has_newline && self.line.last() == Some(&b'\r') {
            self.line.pop();
        }
        Some(LineRead::Line(self.line))
    }
}

/// Read one line of at most `limit_bytes` (see [`LineAccumulator`]).
pub(crate) fn read_line_bounded<R: BufRead>(
    reader: &mut R,
    limit_bytes: usize,
) -> io::Result<Option<LineRead>> {
    let mut line = LineAccumulator::new(limit_bytes);
    loop {
        let chunk = match reader.fill_buf() {
            Ok(chunk) => chunk,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if chunk.is_empty() {
            return Ok(line.finish());
        }
        let (consumed, complete) = line.feed(chunk);
        reader.consume(consumed);
        if complete {
            return Ok(line.finish());
        }
    }
}

/// `BufRead::lines`, with each line limited to [`LINE_LIMIT_BYTES`]: a
/// longer one is skipped, with a note on stderr naming `who`, rather than
/// held in memory.  Invalid UTF-8 is an error, as with `lines`.
pub(crate) fn bounded_lines<R: BufRead>(
    reader: R,
    who: &'static str,
) -> impl Iterator<Item = io::Result<String>> {
    let mut reader = reader;
    std::iter::from_fn(move || {
        loop {
            match read_line_bounded(&mut reader, LINE_LIMIT_BYTES) {
                Ok(None) => return None,
                Ok(Some(LineRead::Line(bytes))) => {
                    return Some(
                        String::from_utf8(bytes)
                            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
                    );
                }
                Ok(Some(LineRead::TooLong { length_bytes })) => {
                    eprintln!(
                        "[{who}] skipped a line of {length_bytes} bytes: over the {LINE_LIMIT_BYTES}-byte limit"
                    );
                }
                Err(e) => return Some(Err(e)),
            }
        }
    })
}

/// The JSON-RPC errors the adapter returns.
#[derive(Debug)]
pub(crate) struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn auth_required() -> Self {
        RpcError {
            code: -32000,
            message: "Authentication required".into(),
            data: None,
        }
    }
    pub fn internal(detail: impl Into<String>) -> Self {
        let detail = detail.into();
        RpcError {
            code: -32603,
            message: format!("Internal error: {detail}"),
            data: Some(json!({ "details": detail })),
        }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        RpcError {
            code: -32603,
            message: message.into(),
            data: None,
        }
    }
    pub fn not_found(method: &str) -> Self {
        RpcError {
            code: -32601,
            message: format!("Method not found: {method}"),
            data: None,
        }
    }
}

pub(crate) type RpcResult = Result<Value, RpcError>;

/// Writes JSON lines to one pipe, whole lines at a time.
pub(crate) struct Lines<W: Write>(Mutex<W>);

impl<W: Write> Lines<W> {
    pub fn new(w: W) -> Self {
        Lines(Mutex::new(w))
    }

    pub fn send(&self, v: &Value) {
        let mut w = self
            .0
            .lock()
            .expect("pipe lock: no writer panics while holding it");
        // A write fails only once the peer has exited; the thread reading its
        // other pipe then sees EOF and ends the session, so there is nothing
        // more to do with the error here.
        let _ = writeln!(w, "{v}");
        let _ = w.flush();
    }
}

/// The ACP client (Axel's session host), over our stdout.
pub(crate) struct Client {
    pub out: Lines<std::io::Stdout>,
    pub next_id: AtomicU64,
    pub pending: Mutex<HashMap<u64, Sender<Result<Value, Value>>>>,
}

impl Client {
    pub fn respond(&self, id: &Value, result: RpcResult) {
        let msg = match result {
            Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
            Err(e) => {
                let mut err = json!({ "code": e.code, "message": e.message });
                if let Some(d) = e.data {
                    err["data"] = d;
                }
                json!({ "jsonrpc": "2.0", "id": id, "error": err })
            }
        };
        self.out.send(&msg);
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.out
            .send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    pub fn update(&self, session_id: &str, update: Value) {
        self.notify(
            "session/update",
            json!({ "sessionId": session_id, "update": update }),
        );
    }

    /// A request to the client; blocks until it answers.  Deliberately
    /// without a timeout: permission requests wait for a human, and the wait
    /// ends with the process when the client closes our stdin.
    pub fn request(&self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        let previous = self.pending.lock().expect("pending lock").insert(id, tx);
        debug_assert!(
            previous.is_none(),
            "request ids come from a counter: never reused"
        );
        self.out
            .send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        rx.recv()
            .unwrap_or_else(|_| Err(json!({ "message": "client went away" })))
    }

    pub fn on_response(&self, msg: &Value) {
        let Some(id) = msg.get("id").and_then(Value::as_u64) else {
            return;
        };
        if let Some(tx) = self.pending.lock().expect("pending lock").remove(&id) {
            let r = match msg.get("error") {
                Some(e) => Err(e.clone()),
                None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
            };
            // The requester stopped waiting (its thread ended): nobody to tell.
            let _ = tx.send(r);
        }
    }
}

impl Client {
    pub fn stdio() -> Self {
        Client {
            out: Lines::new(std::io::stdout()),
            next_id: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(input: &[u8], limit_bytes: usize) -> Vec<LineRead> {
        // A tiny buffer, so lines span several chunks.
        let mut reader = io::BufReader::with_capacity(3, input);
        std::iter::from_fn(|| read_line_bounded(&mut reader, limit_bytes).unwrap()).collect()
    }

    #[test]
    fn lines_split_like_bufread_lines() {
        let line = |s: &str| LineRead::Line(s.as_bytes().to_vec());
        assert_eq!(
            read_all(b"one\r\n\ntwo\nthree", 100),
            vec![line("one"), line(""), line("two"), line("three")]
        );
        assert_eq!(read_all(b"", 100), vec![]);
        assert_eq!(
            read_all(b"cr only\r", 100),
            vec![line("cr only\r")],
            "a lone \\r stays, as with `lines`"
        );
    }

    #[test]
    fn a_line_over_the_limit_is_skipped_and_the_next_is_read() {
        let lines = read_all(b"short\nmuch too long\nok\nexactly10!\n", 10);
        assert_eq!(
            lines,
            vec![
                LineRead::Line(b"short".to_vec()),
                LineRead::TooLong { length_bytes: 13 },
                LineRead::Line(b"ok".to_vec()),
                LineRead::Line(b"exactly10!".to_vec()),
            ]
        );
    }

    #[test]
    fn bounded_lines_skips_long_lines_and_rejects_bad_utf8() {
        let long = "x".repeat(LINE_LIMIT_BYTES + 1);
        let input = format!("a\n{long}\nb\n");
        let got: Vec<String> = bounded_lines(input.as_bytes(), "test")
            .map(Result::unwrap)
            .collect();
        assert_eq!(got, vec!["a", "b"]);
        assert!(
            bounded_lines(&b"\xff\n"[..], "test")
                .next()
                .unwrap()
                .is_err()
        );
    }
}
