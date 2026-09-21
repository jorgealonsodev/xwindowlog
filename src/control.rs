//! Unix listener, `SO_PEERCRED` check, request/response JSON, transient-client lifecycle (D-5).
//!
//! RF-49's `pause`/`resume` control channel: a `SOCK_STREAM` listener in `$XDG_RUNTIME_DIR`, one
//! request per connection, `SO_PEERCRED`-checked before any read (design §2 D-5, §5). This
//! module owns the raw fd-level building blocks — bind, accept, credential check, bounded read,
//! wire types — and the thin CLI-side sender. `reactor.rs` (Phase 14) is what polls the listener
//! and transient client fds; this module is unit-tested against a real `UnixListener`, entirely
//! independent of that poll loop (task 13.8).

use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
use nix::unistd::Uid;
use serde::{Deserialize, Serialize};

/// Wire protocol version this daemon speaks (design §5).
const PROTOCOL_VERSION: u8 = 1;

/// Request body cap; a line missing its trailing `\n` within this many bytes is rejected
/// (design §7 threat matrix, §5 wire-protocol constraints).
pub const MAX_REQUEST_BYTES: usize = 4096;

/// At most this many clients hold a transient fd at once; the next one is accepted and
/// immediately closed (design §5, D-5).
pub const MAX_CONCURRENT_CLIENTS: usize = 4;

/// A client that sends no complete line within this deadline is dropped (design §5).
pub const CLIENT_DEADLINE: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------------------------
// Wire types (design §5)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    Pause {
        #[serde(default)]
        minutes: Option<u32>,
    },
    Resume,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Envelope {
    pub v: u8,
    #[serde(flatten)]
    pub req: Request,
}

// `Response` also derives `Deserialize` (beyond design §5's literal snippet, which only shows
// the daemon's own encode path) because the CLI client below decodes exactly this type from
// the daemon's reply; the wire shape is unchanged.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Response {
    Ok {
        v: u8,
        ok: bool,
        state: String,
        until: Option<String>,
    },
    Err {
        v: u8,
        ok: bool,
        error: ErrCode,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrCode {
    UnsupportedVersion,
    Malformed,
    AlreadyPaused,
    NotPaused,
    Internal,
}

impl Response {
    fn err(error: ErrCode, message: impl Into<String>) -> Self {
        Response::Err {
            v: PROTOCOL_VERSION,
            ok: false,
            error,
            message: message.into(),
        }
    }
}

/// Whether the daemon currently has an open `paused` interval (design §5's `AlreadyPaused`/
/// `NotPaused` distinction). Owned by whichever caller tracks real state; Phase 13 exposes only
/// the pure decision in [`handle_request`], not the tracker/store wiring (task 13.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseState {
    Active,
    Paused,
}

/// Pure decision: given the current [`PauseState`], what does this [`Request`] answer with.
/// Carries no I/O and no `store` dependency — the actual `intervals` write happens in
/// `reactor.rs` once it has decided to accept the request (design §2 D-1: the daemon is the
/// sole writer of `intervals`).
pub fn handle_request(req: &Request, state: PauseState) -> Response {
    match (req, state) {
        (Request::Pause { .. }, PauseState::Paused) => {
            Response::err(ErrCode::AlreadyPaused, "daemon is already paused")
        }
        (Request::Pause { .. }, PauseState::Active) => Response::Ok {
            v: PROTOCOL_VERSION,
            ok: true,
            state: "paused".to_string(),
            // The wall-clock `until` deadline is computed once `Deadlines::PauseExpiry`
            // (Phase 14) exists; Phase 13 only decides pause/not-paused, never the expiry.
            until: None,
        },
        (Request::Resume, PauseState::Active) => {
            Response::err(ErrCode::NotPaused, "daemon is not paused")
        }
        (Request::Resume, PauseState::Paused) => Response::Ok {
            v: PROTOCOL_VERSION,
            ok: true,
            state: "active".to_string(),
            until: None,
        },
    }
}

// ---------------------------------------------------------------------------------------------
// Listener lifecycle (task 13.5)
// ---------------------------------------------------------------------------------------------

/// Joins the socket file name onto a caller-supplied runtime directory. The caller resolves
/// `$XDG_RUNTIME_DIR` (Phase 15+); this module never reads the environment itself.
pub fn socket_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("xwindowlog.sock")
}

/// Binds the control socket, unlinking a stale path first. Safe to call unconditionally
/// because the caller only reaches this after `flock` on the lock file has already proved no
/// live instance holds it (design §2 D-5 "Lifecycle") — a `SIGKILL`ed predecessor's leftover
/// socket inode is not a live listener.
pub fn bind(path: &Path) -> io::Result<UnixListener> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    UnixListener::bind(path)
}

// ---------------------------------------------------------------------------------------------
// Accept + credential + capacity (tasks 13.4c, 13.4d, 13.5)
// ---------------------------------------------------------------------------------------------

/// Outcome of accepting one connection off the listener.
#[derive(Debug)]
pub enum Accepted {
    /// Accepted and credential-checked; ready for [`service_connection`].
    Client(UnixStream),
    /// The peer's `SO_PEERCRED` uid did not match; closed without reading any bytes.
    RejectedUid,
    /// The `MAX_CONCURRENT_CLIENTS`-th+1 concurrent client; accepted and immediately closed.
    RejectedCapacity,
}

/// Accepts one connection and applies D-5's two independent bounds: capacity, then the
/// credential check. `active_clients` is the caller's own count of fds it currently holds open
/// for other clients — this module does not track it, since that bookkeeping belongs to
/// whichever caller owns the fd table (`reactor.rs`, Phase 14).
pub fn accept<W: Write>(
    listener: &UnixListener,
    active_clients: usize,
    own_uid: Uid,
    log: &mut W,
) -> io::Result<Accepted> {
    let (stream, _addr) = listener.accept()?;
    if active_clients >= MAX_CONCURRENT_CLIENTS {
        return Ok(Accepted::RejectedCapacity);
    }
    let peer_uid = peer_uid(&stream)?;
    if peer_uid != own_uid {
        writeln!(
            log,
            "xwindowlog: rejected control connection from uid {} (expected {}); closed without reading",
            peer_uid.as_raw(),
            own_uid.as_raw()
        )?;
        return Ok(Accepted::RejectedUid);
    }
    Ok(Accepted::Client(stream))
}

fn peer_uid(stream: &UnixStream) -> io::Result<Uid> {
    let creds = getsockopt(stream, PeerCredentials).map_err(io::Error::from)?;
    Ok(Uid::from_raw(creds.uid()))
}

// ---------------------------------------------------------------------------------------------
// Bounded read + request handling (tasks 13.4a, 13.4b, 13.4e, 13.4f, 13.5)
// ---------------------------------------------------------------------------------------------

enum RequestError {
    /// No trailing `\n` arrived within `CLIENT_DEADLINE`.
    Timeout,
    /// More than `MAX_REQUEST_BYTES` arrived without a trailing `\n`.
    Oversized,
    Io(io::Error),
}

/// Reads one `\n`-terminated line, bounded by both size and total elapsed time. The deadline is
/// tracked against a single [`Instant`] and re-applied on every `read()` call, so a peer that
/// trickles bytes in slowly cannot extend the 1s budget by resetting a per-call timeout
/// (rust-testing skill: every blocking read on this socket must be covered, not just the
/// first).
fn read_request_line(stream: &mut UnixStream, deadline: Duration) -> Result<Vec<u8>, RequestError> {
    let start = Instant::now();
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let elapsed = start.elapsed();
        if elapsed >= deadline {
            return Err(RequestError::Timeout);
        }
        stream
            .set_read_timeout(Some(deadline - elapsed))
            .map_err(RequestError::Io)?;
        match stream.read(&mut byte) {
            Ok(0) => return Err(RequestError::Timeout),
            Ok(_) if byte[0] == b'\n' => return Ok(line),
            Ok(_) => {
                line.push(byte[0]);
                if line.len() > MAX_REQUEST_BYTES {
                    return Err(RequestError::Oversized);
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Err(RequestError::Timeout)
            }
            Err(e) => return Err(RequestError::Io(e)),
        }
    }
}

#[derive(Deserialize)]
struct VersionOnly {
    v: u8,
}

/// Parses `v` before the internally-tagged `cmd` body, so a future-version request answers
/// `UnsupportedVersion` even when its body isn't valid v1 grammar (R3) — `{"v":2}` and
/// `{"v":2,"cmd":"<future>"}` never reach the full `Envelope` decode below.
/// One non-blocking attempt to advance a request line, for a future poll-driven reactor
/// (Phase 14) to call between servicing other fds instead of blocking this thread on one
/// connection (R4). Chosen over further shrinking `CLIENT_DEADLINE`, which would still block
/// the caller, just for less time. Residual bound: this call itself never blocks, but the
/// caller still owns re-polling before its own deadline elapses — `reactor.rs` (Phase 14),
/// unwritten, is what will drive that loop.
pub fn try_read_request_line(
    stream: &mut UnixStream,
    partial: &mut Vec<u8>,
) -> io::Result<Option<Vec<u8>>> {
    stream.set_nonblocking(true)?;
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed")),
            Ok(_) if byte[0] == b'\n' => return Ok(Some(std::mem::take(partial))),
            Ok(_) => partial.push(byte[0]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(e),
        }
    }
}

/// `pub(crate)` so `reactor.rs` (Phase 14) can reuse this already-tested version-before-body
/// decode when it drives `try_read_request_line` itself, instead of re-implementing R3's
/// ordering (task 14's CRITICAL fix).
pub(crate) fn parse_envelope(line: &[u8]) -> Result<Envelope, Response> {
    let VersionOnly { v } = serde_json::from_slice(line)
        .map_err(|e| Response::err(ErrCode::Malformed, e.to_string()))?;
    if v != PROTOCOL_VERSION {
        return Err(Response::err(
            ErrCode::UnsupportedVersion,
            format!("unsupported protocol version {v}"),
        ));
    }
    serde_json::from_slice(line).map_err(|e| Response::err(ErrCode::Malformed, e.to_string()))
}

fn write_response(stream: &mut UnixStream, response: &Response) -> io::Result<()> {
    let mut body = serde_json::to_vec(response).map_err(io::Error::other)?;
    body.push(b'\n');
    stream.write_all(&body)
}

/// The outcome of servicing one already-accepted connection end to end.
#[derive(Debug)]
pub enum Serviced {
    /// A request line arrived, was decided, and the client got a reply.
    Responded(Response),
    /// The peer never produced a bounded, `\n`-terminated request; closed with no reply
    /// (oversized/unterminated body, or the 1s deadline).
    ClosedWithoutReply,
}

/// Services one connection [`accept`] already credential-checked: read the bounded request
/// line, decide against `state`, and reply. Never touches `store`; the caller (`reactor.rs`)
/// owns the actual `intervals` write once it accepts the decided [`Response`].
pub fn service_connection<W: Write>(
    mut stream: UnixStream,
    state: PauseState,
    log: &mut W,
) -> io::Result<Serviced> {
    let line = match read_request_line(&mut stream, CLIENT_DEADLINE) {
        Ok(line) => line,
        Err(RequestError::Timeout) => {
            writeln!(
                log,
                "xwindowlog: control client sent no request within 1s; dropping"
            )?;
            return Ok(Serviced::ClosedWithoutReply);
        }
        Err(RequestError::Oversized) => {
            writeln!(
                log,
                "xwindowlog: control client's request exceeded {MAX_REQUEST_BYTES} bytes without a newline; dropping"
            )?;
            return Ok(Serviced::ClosedWithoutReply);
        }
        Err(RequestError::Io(e)) => return Err(e),
    };

    let response = match parse_envelope(&line) {
        Ok(envelope) => handle_request(&envelope.req, state),
        Err(response) => response,
    };
    write_response(&mut stream, &response)?;
    Ok(Serviced::Responded(response))
}

// ---------------------------------------------------------------------------------------------
// CLI client (RF-49; tasks 13.6, 13.7). The pause/resume CLI processes only send a request and
// read the daemon's reply over this socket (daemon-lifecycle scenario: pause/resume clients
// never write intervals directly). This section's module boundary is enforced below by a
// textual scan of this exact block for any dependency on the interval-persistence module.
// ---------------------------------------------------------------------------------------------

/// Sends `Pause` to the daemon listening at `socket_path` and returns its decoded reply.
pub fn send_pause(socket_path: &Path, minutes: Option<u32>) -> io::Result<Response> {
    send_request(socket_path, Request::Pause { minutes })
}

/// Sends `Resume` to the daemon listening at `socket_path` and returns its decoded reply.
pub fn send_resume(socket_path: &Path) -> io::Result<Response> {
    send_request(socket_path, Request::Resume)
}

fn send_request(socket_path: &Path, req: Request) -> io::Result<Response> {
    let mut stream = UnixStream::connect(socket_path)?;
    // R4: the daemon bounds its peer at CLIENT_DEADLINE but gave this client none, so a
    // busy-not-crashed daemon left it blocked forever; bound both directions here too.
    stream.set_read_timeout(Some(CLIENT_DEADLINE))?;
    stream.set_write_timeout(Some(CLIENT_DEADLINE))?;
    let envelope = Envelope {
        v: PROTOCOL_VERSION,
        req,
    };
    let mut body = serde_json::to_vec(&envelope).map_err(io::Error::other)?;
    body.push(b'\n');
    stream.write_all(&body)?;

    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    let response: Response = serde_json::from_str(reply.trim_end()).map_err(io::Error::other)?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;

    fn unique_socket_path(case: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "xwindowlog-test-control-{case}-{}-{n}.sock",
            std::process::id()
        ))
    }

    fn spawn_test_listener(case: &str) -> (UnixListener, PathBuf) {
        let path = unique_socket_path(case);
        let listener = bind(&path).expect("bind must succeed on a fresh path");
        (listener, path)
    }

    /// Reads whatever the peer sends until it closes. A server that closes a connection while
    /// unread bytes are still queued (e.g. the client's own oversized payload it never fully
    /// consumed) makes Linux send `RST` instead of a graceful `FIN`; both are valid ways to
    /// observe "no reply arrived", so this treats `ConnectionReset` the same as a clean EOF.
    fn read_reply_tolerant_of_reset(stream: &mut UnixStream) -> Vec<u8> {
        let mut reply = Vec::new();
        match stream.read_to_end(&mut reply) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            Err(e) => panic!("unexpected read error: {e}"),
        }
        reply
    }

    // --- 13.4a: oversized / no-trailing-newline body is rejected ---------------------------

    #[test]
    fn oversized_request_without_trailing_newline_is_rejected() {
        let (listener, path) = spawn_test_listener("oversized");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut log = Vec::new();
            let outcome = service_connection(stream, PauseState::Active, &mut log).unwrap();
            (outcome, log)
        });

        let mut client = UnixStream::connect(&path).unwrap();
        let payload = vec![b'x'; MAX_REQUEST_BYTES + 100];
        client.write_all(&payload).unwrap();

        let reply = read_reply_tolerant_of_reset(&mut client);
        assert!(
            reply.is_empty(),
            "an oversized/unterminated request must get no reply"
        );
        let (outcome, log) = server.join().unwrap();
        assert!(matches!(outcome, Serviced::ClosedWithoutReply));
        // Distinguishes this from the 13.4e timeout path, which would also close without a
        // reply but logs a different message — proving the size cap itself fired, not the
        // 1s deadline racing it.
        let log_text = String::from_utf8(log).unwrap();
        assert!(
            log_text.contains("exceeded") && log_text.contains("bytes"),
            "the oversized cap specifically must be what rejected this request: {log_text}"
        );
        let _ = std::fs::remove_file(&path);
    }

    // --- 13.4b: `v != 1` returns `UnsupportedVersion` ---------------------------------------

    #[test]
    fn unsupported_protocol_version_returns_unsupported_version() {
        let (listener, path) = spawn_test_listener("badversion");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            service_connection(stream, PauseState::Active, &mut io::sink()).unwrap()
        });

        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(b"{\"v\":2,\"cmd\":\"resume\"}\n").unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(reply.trim_end()).unwrap();
        assert_eq!(parsed["error"], "unsupported_version");
        assert_eq!(parsed["ok"], false);
        assert!(matches!(
            server.join().unwrap(),
            Serviced::Responded(Response::Err {
                error: ErrCode::UnsupportedVersion,
                ..
            })
        ));
        let _ = std::fs::remove_file(&path);
    }

    // --- R4: a non-blocking, resumable read path a future poll-driven reactor can drive -----

    #[test]
    fn try_read_request_line_never_blocks_and_resumes_across_calls() {
        let (listener, path) = spawn_test_listener("nonblocking");
        let mut client = UnixStream::connect(&path).unwrap();
        let (mut server_stream, _) = listener.accept().unwrap();
        let mut partial = Vec::new();

        let start = Instant::now();
        assert!(matches!(
            try_read_request_line(&mut server_stream, &mut partial),
            Ok(None)
        ));
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "an attempt with no data waiting must never block on the read"
        );

        client.write_all(b"ab").unwrap();
        thread::sleep(Duration::from_millis(50));
        assert!(matches!(
            try_read_request_line(&mut server_stream, &mut partial),
            Ok(None)
        ));
        assert_eq!(partial, b"ab", "partial bytes must persist across calls");

        client.write_all(b"c\n").unwrap();
        thread::sleep(Duration::from_millis(50));
        let line = try_read_request_line(&mut server_stream, &mut partial)
            .unwrap()
            .expect("a complete line must be returned once the newline arrives");
        assert_eq!(line, b"abc");
        let _ = std::fs::remove_file(&path);
    }

    // --- R3: version is checked before the body is decoded, not only when v1 grammar matches

    #[test]
    fn unsupported_version_wins_over_a_missing_cmd_field() {
        let (listener, path) = spawn_test_listener("v2-nocmd");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            service_connection(stream, PauseState::Active, &mut io::sink()).unwrap()
        });
        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(b"{\"v\":2}\n").unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(reply.trim_end()).unwrap();
        assert_eq!(parsed["error"], "unsupported_version");
        assert!(matches!(
            server.join().unwrap(),
            Serviced::Responded(Response::Err {
                error: ErrCode::UnsupportedVersion,
                ..
            })
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unsupported_version_wins_over_an_unrecognized_future_cmd() {
        let (listener, path) = spawn_test_listener("v2-futurecmd");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            service_connection(stream, PauseState::Active, &mut io::sink()).unwrap()
        });
        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(b"{\"v\":2,\"cmd\":\"snooze\"}\n").unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(reply.trim_end()).unwrap();
        assert_eq!(parsed["error"], "unsupported_version");
        assert!(matches!(
            server.join().unwrap(),
            Serviced::Responded(Response::Err {
                error: ErrCode::UnsupportedVersion,
                ..
            })
        ));
        let _ = std::fs::remove_file(&path);
    }

    // --- 13.4c: a connecting peer whose uid differs is closed without being read -----------

    #[test]
    fn different_uid_peer_is_rejected_without_reading_and_logged() {
        let (listener, path) = spawn_test_listener("uidmismatch");
        // A uid that cannot be this test process's own effective uid.
        let wrong_uid = Uid::from_raw(Uid::effective().as_raw().wrapping_add(1));

        let server = thread::spawn(move || {
            let mut log = Vec::new();
            let outcome = accept(&listener, 0, wrong_uid, &mut log).unwrap();
            (outcome, log)
        });

        let mut client = UnixStream::connect(&path).unwrap();
        // Even a well-formed request must get no reply: the rejection happens before any read.
        client.write_all(b"{\"v\":1,\"cmd\":\"resume\"}\n").unwrap();
        let reply = read_reply_tolerant_of_reset(&mut client);
        assert!(
            reply.is_empty(),
            "a uid-mismatched peer must get no reply at all"
        );

        let (outcome, log) = server.join().unwrap();
        assert!(matches!(outcome, Accepted::RejectedUid));
        let log_text = String::from_utf8(log).unwrap();
        assert!(
            log_text.contains("rejected"),
            "the rejection must be logged: {log_text}"
        );
        let _ = std::fs::remove_file(&path);
    }

    // --- 13.4d: a 5th concurrent client is accepted and immediately closed -----------------

    #[test]
    fn fifth_concurrent_client_is_accepted_and_immediately_closed() {
        let (listener, path) = spawn_test_listener("fifthclient");
        let own_uid = Uid::effective();
        let mut log = io::sink();

        // Four real clients connect and stay open before any accept() runs, so every accept()
        // below has a pending connection queued and never blocks.
        let _clients: Vec<UnixStream> = (0..MAX_CONCURRENT_CLIENTS)
            .map(|_| UnixStream::connect(&path).unwrap())
            .collect();

        for i in 0..MAX_CONCURRENT_CLIENTS {
            let outcome = accept(&listener, i, own_uid, &mut log).unwrap();
            assert!(
                matches!(outcome, Accepted::Client(_)),
                "client #{i} must be accepted under the concurrency cap"
            );
        }

        let mut fifth_client = UnixStream::connect(&path).unwrap();
        let fifth_outcome = accept(&listener, MAX_CONCURRENT_CLIENTS, own_uid, &mut log).unwrap();
        assert!(matches!(fifth_outcome, Accepted::RejectedCapacity));

        let mut reply = Vec::new();
        fifth_client.read_to_end(&mut reply).unwrap();
        assert!(
            reply.is_empty(),
            "the 5th client must be closed with no reply"
        );
        let _ = std::fs::remove_file(&path);
    }

    // --- 13.4e: a client sending no line within 1s is dropped ------------------------------

    #[test]
    fn client_sending_no_line_within_one_second_is_dropped() {
        let (listener, path) = spawn_test_listener("timeout");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            service_connection(stream, PauseState::Active, &mut io::sink()).unwrap()
        });

        let client = UnixStream::connect(&path).unwrap();
        let start = Instant::now();
        let outcome = server.join().unwrap();
        let elapsed = start.elapsed();

        assert!(matches!(outcome, Serviced::ClosedWithoutReply));
        assert!(
            elapsed >= Duration::from_millis(900),
            "the deadline fired too early: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "the deadline must be roughly 1s, not unbounded"
        );
        drop(client);
        let _ = std::fs::remove_file(&path);
    }

    // --- 13.4f: Pause-while-paused / Resume-while-not-paused return state errors -----------

    #[test]
    fn pause_while_paused_and_resume_while_not_paused_return_state_errors() {
        let (listener, path) = spawn_test_listener("statecheck-pause");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            service_connection(stream, PauseState::Paused, &mut io::sink()).unwrap()
        });
        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(b"{\"v\":1,\"cmd\":\"pause\"}\n").unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(reply.trim_end()).unwrap();
        assert_eq!(parsed["error"], "already_paused");
        assert!(matches!(
            server.join().unwrap(),
            Serviced::Responded(Response::Err {
                error: ErrCode::AlreadyPaused,
                ..
            })
        ));
        let _ = std::fs::remove_file(&path);

        let (listener2, path2) = spawn_test_listener("statecheck-resume");
        let server2 = thread::spawn(move || {
            let (stream, _) = listener2.accept().unwrap();
            service_connection(stream, PauseState::Active, &mut io::sink()).unwrap()
        });
        let mut client2 = UnixStream::connect(&path2).unwrap();
        client2
            .write_all(b"{\"v\":1,\"cmd\":\"resume\"}\n")
            .unwrap();
        let mut reply2 = String::new();
        client2.read_to_string(&mut reply2).unwrap();
        let parsed2: serde_json::Value = serde_json::from_str(reply2.trim_end()).unwrap();
        assert_eq!(parsed2["error"], "not_paused");
        assert!(matches!(
            server2.join().unwrap(),
            Serviced::Responded(Response::Err {
                error: ErrCode::NotPaused,
                ..
            })
        ));
        let _ = std::fs::remove_file(&path2);
    }

    // --- R4: the CLI client bounds its own wait; a silent peer cannot hold it indefinitely --

    #[test]
    fn cli_client_read_times_out_against_a_silent_peer() {
        let (listener, path) = spawn_test_listener("clienttimeout");
        let server = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            // Accepts and holds the connection open without ever replying — outlives the
            // client's own read deadline, simulating an alive-but-not-servicing daemon.
            thread::sleep(Duration::from_secs(3));
        });
        let start = Instant::now();
        let result = send_resume(&path);
        let elapsed = start.elapsed();
        assert!(
            result.is_err(),
            "a silent peer must not be able to fabricate a reply"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "the client must give up within its own read deadline, not wait on the peer: {elapsed:?}"
        );
        let _ = std::fs::remove_file(&path);
        drop(server);
    }

    // --- 13.6: the CLI client section above never depends on `store` -----------------------

    #[test]
    fn cli_client_never_depends_on_store() {
        let source = include_str!("control.rs");
        let block_start = source
            .find("// CLI client (RF-49; tasks 13.6, 13.7)")
            .expect("control.rs defines the CLI client section (task 13.7)");
        let section_end = source[block_start..]
            .find("#[cfg(test)]")
            .expect("the CLI client section is followed by the test module");
        let block = &source[block_start..block_start + section_end];
        // R2: scan every `use` line in the WHOLE file, not a substring of this slice — the
        // missed case was a `use crate::store::...;` sitting with the other imports at the top
        // of the file, outside the old slice. Token-matched (so it can't trip on doc-comment
        // prose elsewhere that merely mentions `store`) and independent of the banner comment
        // text, so rewording it can't defeat this check either.
        let mentions_store = source
            .lines()
            .filter(|l| l.trim_start().starts_with("use "))
            .any(|l| {
                l.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .any(|tok| tok.eq_ignore_ascii_case("store"))
            });
        assert!(
            !mentions_store,
            "this module (including the CLI client) must never depend on crate::store \
             (daemon-lifecycle: pause/resume clients never write intervals directly)"
        );
        assert!(
            block.contains("fn send_pause"),
            "the client section must define send_pause"
        );
        assert!(
            block.contains("fn send_resume"),
            "the client section must define send_resume"
        );
    }
}
