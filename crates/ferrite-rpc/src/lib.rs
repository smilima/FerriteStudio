//! Content-Length framed JSON over a child process's stdin and stdout.
//!
//! The Language Server Protocol and the Debug Adapter Protocol use the same
//! framing: a `Content-Length: N` header, a blank line, then N bytes of
//! JSON. This crate spawns the server, writes messages, and runs a reader
//! thread that hands every incoming message to a callback.

use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::Value;

/// Writes one framed message.
pub fn write_message<W: Write>(w: &mut W, msg: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(&body)?;
    w.flush()
}

/// Reads one framed message; `Ok(None)` at end of stream.
pub fn read_message<R: BufRead>(r: &mut R) -> io::Result<Option<Value>> {
    let mut length: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if length.is_some() {
                break;
            }
            continue; // tolerate stray blank lines between messages
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().ok();
            }
        }
    }
    let len = length.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length"))?;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Hides the console window a GUI process would otherwise open for a
/// console child on Windows. A no-op elsewhere.
pub fn no_console(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// A running server process that speaks framed JSON.
pub struct Endpoint {
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
}

impl Endpoint {
    /// Spawns `cmd` and starts the reader threads. `on_message` runs on the
    /// reader thread for every message; `on_stderr` gets each stderr line;
    /// `on_exit` runs once when stdout closes.
    pub fn spawn(
        mut cmd: Command,
        on_message: impl Fn(Value) + Send + 'static,
        on_stderr: impl Fn(String) + Send + 'static,
        on_exit: impl FnOnce() + Send + 'static,
    ) -> io::Result<Arc<Endpoint>> {
        no_console(&mut cmd);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        thread::Builder::new().name("rpc-stderr".into()).spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                on_stderr(line);
            }
        })?;
        thread::Builder::new().name("rpc-reader".into()).spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(Some(msg)) = read_message(&mut reader) {
                on_message(msg);
            }
            on_exit();
        })?;

        Ok(Arc::new(Endpoint { stdin: Mutex::new(stdin), child: Mutex::new(child) }))
    }

    pub fn send(&self, msg: &Value) -> io::Result<()> {
        let mut stdin = self.stdin.lock().unwrap_or_else(|e| e.into_inner());
        write_message(&mut *stdin, msg)
    }

    /// Kills the process if it is still running.
    pub fn kill(&self) {
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        let _ = child.kill();
        let _ = child.wait();
    }

    pub fn has_exited(&self) -> bool {
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        !matches!(child.try_wait(), Ok(None))
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        write_message(&mut buf, &json!({"jsonrpc": "2.0", "id": 1, "method": "ping"})).unwrap();
        write_message(&mut buf, &json!({"seq": 2, "text": "héllo"})).unwrap();
        let mut r = io::Cursor::new(buf);
        let a = read_message(&mut r).unwrap().unwrap();
        let b = read_message(&mut r).unwrap().unwrap();
        assert_eq!(a["method"], "ping");
        assert_eq!(b["text"], "héllo");
        assert!(read_message(&mut r).unwrap().is_none());
    }

    #[test]
    fn extra_headers_are_ignored() {
        let body = br#"{"ok":true}"#;
        let raw = format!(
            "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\ncontent-length: {}\r\n\r\n",
            body.len()
        );
        let mut data = raw.into_bytes();
        data.extend_from_slice(body);
        let v = read_message(&mut io::Cursor::new(data)).unwrap().unwrap();
        assert_eq!(v["ok"], true);
    }
}
