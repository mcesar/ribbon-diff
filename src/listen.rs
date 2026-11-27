//! `ribbon --listen`: a local socket that lets Claude (or a script) move the view, mark lines,
//! read the files again, and add notes. `ribbon send` is the client.
//!
//! The socket is `.ribbon/ribbon.sock` at the top of the repository. Each request is one line
//! of JSON, and each reply is one line of JSON:
//!
//! ```text
//! {"command":"navigate","path":"src/a.rs","line":40}
//! {"command":"highlight","path":"src/a.rs","range":[40,52]}
//! {"command":"clear"}
//! {"command":"reload"}
//! {"command":"note","path":"src/a.rs","range":[40,52],"summary":"...","rationale":"...","author":"claude"}
//! {"command":"status"}
//! → {"ok":true,...} or {"ok":false,"error":"..."}
//! ```
//!
//! Lines are 1-based. `"side":"old"` means lines of the old version.

use anyhow::{bail, Context};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long a client waits for the viewer, which can be busy (in the editor, for example).
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Scroll to a line, or to the file's title when there is no line.
    Navigate { path: String, old_side: bool, line: Option<usize> },
    /// Mark lines (1-based, inclusive) and scroll to them.
    Highlight { path: String, old_side: bool, range: (usize, usize) },
    /// Remove the mark.
    Clear,
    /// Read the files and the notes again.
    Reload,
    /// Add a note; with `status` "new" and author "review", a finding to fix or dismiss.
    Note { path: String, old_side: bool, range: (usize, usize), summary: String, rationale: Option<String>, author: String, status: Option<String> },
    /// Where the view is, and how many notes are open.
    Status,
}

/// A command from a client, and where to send the reply.
pub struct Request {
    pub command: Command,
    pub reply: mpsc::Sender<Value>,
    /// After this the client has had its error; the viewer drops the request, so a client
    /// that tries again does not get it done twice.
    pub deadline: Instant,
}

pub fn ok(fields: Value) -> Value {
    let mut reply = json!({ "ok": true });
    if let (Some(reply), Value::Object(fields)) = (reply.as_object_mut(), fields) {
        reply.extend(fields);
    }
    reply
}

pub fn error(message: impl Into<String>) -> Value {
    json!({ "ok": false, "error": message.into() })
}

/// Unix socket paths must be shorter than this (104 bytes on macOS, 108 on Linux).
const MAX_SOCKET_PATH: usize = 104;

/// `.ribbon/ribbon.sock` under `root`; when that path is too long for a socket, a short one
/// in `/tmp` named after the folder, which the client finds the same way.
pub fn socket_file(root: &Path) -> PathBuf {
    let path = root.join(".ribbon").join("ribbon.sock");
    if path.as_os_str().len() < MAX_SOCKET_PATH {
        return path;
    }
    // FNV-1a: the same in every build, unlike the standard library's hasher.
    let hash = root.as_os_str().as_encoded_bytes().iter().fold(0xcbf29ce484222325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    PathBuf::from(format!("/tmp/ribbon-{}/{hash:016x}.sock", unsafe { libc::getuid() }))
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty()).map(str::to_owned)
}

fn line_number(value: &Value) -> Option<usize> {
    value.as_u64().filter(|&n| n >= 1).map(|n| n as usize)
}

/// `"range": [a, b]`, or `"line": a` for one line.
fn range(value: &Value) -> anyhow::Result<(usize, usize)> {
    if let Some(range) = value.get("range") {
        return match range.as_array().map(Vec::as_slice) {
            Some([a, b]) => match (line_number(a), line_number(b)) {
                (Some(a), Some(b)) if a <= b => Ok((a, b)),
                _ => bail!("range must be [first, last] with 1-based line numbers"),
            },
            _ => bail!("range must be [first, last]"),
        };
    }
    let line = value.get("line").and_then(line_number).context("needs \"range\": [first, last] or \"line\": N")?;
    Ok((line, line))
}

/// One request line.
pub fn parse(line: &str) -> anyhow::Result<Command> {
    let value: Value = serde_json::from_str(line).context("a request is one line of JSON")?;
    let command = text(&value, "command").context("needs \"command\"")?;
    let path = || text(&value, "path").context("needs \"path\"");
    let old_side = match value.get("side").and_then(Value::as_str) {
        None | Some("new") => false,
        Some("old") => true,
        Some(other) => bail!("side must be \"old\" or \"new\", not {other:?}"),
    };
    Ok(match command.as_str() {
        "navigate" => {
            let line = match value.get("line") {
                None | Some(Value::Null) => None,
                Some(line) => Some(line_number(line).context("line must be a 1-based number")?),
            };
            Command::Navigate { path: path()?, old_side, line }
        }
        "highlight" => Command::Highlight { path: path()?, old_side, range: range(&value)? },
        "clear" => Command::Clear,
        "reload" => Command::Reload,
        "status" => Command::Status,
        "note" | "add note" | "add-note" | "finding" => {
            let finding = command == "finding";
            Command::Note {
                path: path()?,
                old_side,
                range: range(&value)?,
                summary: text(&value, "summary").context("needs \"summary\"")?,
                rationale: text(&value, "rationale"),
                author: text(&value, "author").unwrap_or_else(|| if finding { "review" } else { "claude" }.into()),
                status: text(&value, "status").or_else(|| finding.then(|| "new".into())),
            }
        }
        other => bail!("unknown command {other:?} (navigate, highlight, clear, reload, note, finding, status)"),
    })
}

/// Removes the socket file when the viewer quits.
pub struct Listening {
    path: PathBuf,
}

impl Drop for Listening {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Listen on `path`. A socket file left by a ribbon that is gone is replaced; a live one is
/// an error, so two viewers don't fight over the commands.
pub fn listen(path: &Path) -> anyhow::Result<(mpsc::Receiver<Request>, Listening)> {
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            bail!("another ribbon listens on {} already", path.display());
        }
        std::fs::remove_file(path).with_context(|| format!("cannot remove the old socket {}", path.display()))?;
    }
    let dir = path.parent().context("socket has no folder")?;
    std::fs::create_dir_all(dir)?;
    if dir.file_name().is_some_and(|name| name == ".ribbon") {
        // The folder ignores itself, so git status and --untracked don't show it.
        let ignore = dir.join(".gitignore");
        if !ignore.exists() {
            std::fs::write(&ignore, "*\n")?;
        }
    } else if dir.starts_with("/tmp") {
        // Only this user may connect.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    if path.as_os_str().len() >= MAX_SOCKET_PATH {
        bail!("the socket path {} is too long for a Unix socket; use --listen=SHORTER-PATH", path.display());
    }
    let listener = UnixListener::bind(path).with_context(|| format!("cannot listen on {}", path.display()))?;
    let (sender, requests) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let sender = sender.clone();
            std::thread::spawn(move || serve(stream, sender));
        }
    });
    Ok((requests, Listening { path: path.to_path_buf() }))
}

/// Answer each line of one connection.
fn serve(stream: UnixStream, sender: mpsc::Sender<Request>) {
    let Ok(mut out) = stream.try_clone() else { return };
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match parse(&line) {
            Err(problem) => error(format!("{problem:#}")),
            Ok(command) => {
                let (reply, answer) = mpsc::channel();
                if sender.send(Request { command, reply, deadline: Instant::now() + REPLY_TIMEOUT }).is_err() {
                    return;
                }
                answer.recv_timeout(REPLY_TIMEOUT).unwrap_or_else(|_| error("the viewer did not answer (is it in the editor?)"))
            }
        };
        if writeln!(out, "{reply}").and_then(|()| out.flush()).is_err() {
            return;
        }
    }
}

/// Send one request line and read the reply.
pub fn send(path: &Path, request: &Value) -> anyhow::Result<Value> {
    let mut stream = UnixStream::connect(path).with_context(|| format!("no ribbon listens on {} (start one with ribbon --listen)", path.display()))?;
    stream.set_read_timeout(Some(REPLY_TIMEOUT + Duration::from_secs(1)))?;
    writeln!(stream, "{request}")?;
    stream.flush()?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).context("no reply from ribbon")?;
    serde_json::from_str(&reply).context("ribbon sent a reply that is not JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_requests() {
        assert_eq!(parse(r#"{"command":"navigate","path":"a.rs","line":4}"#).unwrap(), Command::Navigate { path: "a.rs".into(), old_side: false, line: Some(4) });
        assert_eq!(parse(r#"{"command":"navigate","path":"a.rs","side":"old"}"#).unwrap(), Command::Navigate { path: "a.rs".into(), old_side: true, line: None });
        assert_eq!(parse(r#"{"command":"highlight","path":"a.rs","line":7}"#).unwrap(), Command::Highlight { path: "a.rs".into(), old_side: false, range: (7, 7) });
        let Command::Note { author, status, range, .. } = parse(r#"{"command":"finding","path":"a.rs","range":[2,3],"summary":"s"}"#).unwrap() else { panic!() };
        assert_eq!((author.as_str(), status.as_deref(), range), ("review", Some("new"), (2, 3)));
        let Command::Note { author, status, .. } = parse(r#"{"command":"note","path":"a.rs","line":2,"summary":"s"}"#).unwrap() else { panic!() };
        assert_eq!((author.as_str(), status), ("claude", None));
        assert!(parse(r#"{"command":"highlight","path":"a.rs","range":[3,2]}"#).is_err());
        assert!(parse(r#"{"command":"navigate","path":"a.rs","line":0}"#).is_err());
        assert!(parse(r#"{"command":"jump"}"#).is_err());
        assert!(parse("navigate a.rs").is_err());
    }

    #[test]
    fn answers_over_the_socket_and_replaces_a_stale_one() {
        // A short folder: socket paths are limited to about 100 bytes.
        let dir = PathBuf::from(format!("/tmp/ribbon-listen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = socket_file(&dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A socket file nobody listens on.
        drop(UnixListener::bind(&path).unwrap());
        let (requests, guard) = listen(&path).unwrap();
        assert!(listen(&path).is_err());
        std::thread::spawn(move || {
            for request in requests {
                let _ = request.reply.send(ok(json!({ "got": format!("{:?}", request.command) })));
            }
        });
        let reply = send(&path, &json!({ "command": "status" })).unwrap();
        let long = PathBuf::from("/x").join("y".repeat(120));
        assert!(socket_file(&long).starts_with("/tmp") && socket_file(&long) == socket_file(&long));
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["got"], "Status");
        assert_eq!(send(&path, &json!({ "command": "nope" })).unwrap()["ok"], false);
        drop(guard);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
