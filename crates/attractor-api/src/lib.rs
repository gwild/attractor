#![forbid(unsafe_code)]
#![warn(clippy::pedantic, clippy::nursery)]
#![deny(missing_docs, missing_debug_implementations)]

//! # attractor-api
//!
//! Control server + client for the Attractor matrix daemon. The wire types live
//! in [`attractor_proto`] (the portable, both-leg-attested contract); this crate
//! adds the I/O — a JSON-lines request/response protocol over a Unix-domain
//! socket (local, `#[cfg(unix)]`) and TCP (remote).
//!
//! Adopted from stringdriver's `terminal_api`/`sdctl` (Gregory 2026-05-31).
//! **Std-only** — no async runtime — because a control plane that handles one
//! request at a time over a line protocol doesn't need one, which also keeps the
//! MSRV at the 1.92 attestation floor.
//!
//! ## Layering / seam
//!
//! v0 answers only the universal verbs ([`RequestKind::Ping`],
//! [`RequestKind::Status`]). The matrix operations (`Get`/`Insert`/`Link`/
//! `Snapshot`) attach at [`dispatch`] once `attractor-store` lands and the proto
//! verbs are filled jointly — so this crate isn't built ahead of its dependency.
//!
//! ## Fail-loud
//!
//! A malformed or unknown request never panics and never drops silently:
//! [`handle_line`] always returns a serialized [`ApiResponse`], using
//! [`ApiResponse::error`] with a legible reason on bad input (the posture from
//! the #2242 merge-guard review — deny, but tell the caller why).

use attractor_proto::{ApiRequest, ApiResponse, RequestKind};
use serde::Serialize;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, ToSocketAddrs};

/// Environment variable overriding the local Unix-socket path.
pub const ENV_SOCKET_PATH: &str = "ATTRACTOR_SOCKET_PATH";
/// Environment variable overriding the TCP bind/connect address.
pub const ENV_TCP_ADDR: &str = "ATTRACTOR_TCP_ADDR";

/// Resolve the control socket path from `$ATTRACTOR_SOCKET_PATH`, falling back
/// to the proto default. Nothing is hard-coded at a call site — deployments
/// point the daemon + client at the same path via env, no recompile.
#[must_use]
pub fn socket_path() -> String {
    resolve(std::env::var(ENV_SOCKET_PATH).ok(), attractor_proto::SOCKET_PATH)
}

/// Resolve the TCP bind/connect address from `$ATTRACTOR_TCP_ADDR`, falling back
/// to `0.0.0.0:<proto TCP_PORT>`.
#[must_use]
pub fn tcp_addr() -> String {
    let default = format!("0.0.0.0:{}", attractor_proto::TCP_PORT);
    resolve(std::env::var(ENV_TCP_ADDR).ok(), &default)
}

/// Pure resolver: a non-empty override wins, otherwise the default. Split out
/// from the env read so it's testable without mutating process-global env
/// (which would race other tests).
#[must_use]
fn resolve(override_val: Option<String>, default: &str) -> String {
    match override_val {
        Some(v) if !v.trim().is_empty() => v,
        _ => default.to_owned(),
    }
}

/// Payload for a [`RequestKind::Status`] reply (the server decides this shape,
/// per the proto's `data: Value` contract).
#[derive(Debug, Clone, Serialize)]
pub struct StatusData {
    /// Best-effort daemon uptime in seconds.
    pub uptime_secs: u64,
    /// Whether a store backend (`attractor-store`) is attached yet.
    pub store_attached: bool,
}

/// Pure request → response dispatch. No I/O, so it is trivially testable and is
/// where `attractor-store`'s matrix-op handlers will graft in. The request's
/// correlation `id` (if any) is echoed onto the response.
#[must_use]
pub fn dispatch(req: &ApiRequest, uptime_secs: u64) -> ApiResponse {
    let resp = match req.kind {
        RequestKind::Ping => ApiResponse::ok_empty(),
        RequestKind::Status => ApiResponse::ok(StatusData {
            uptime_secs,
            store_attached: false,
        }),
        // `RequestKind` is #[non_exhaustive]: a verb this build doesn't know
        // (a future matrix op from a newer client) is answered, not dropped.
        _ => ApiResponse::error("unsupported request verb for this daemon version"),
    };
    match req.id {
        Some(id) => resp.with_id(id),
        None => resp,
    }
}

/// Parse one JSON request line, dispatch it, and serialize the response line.
///
/// Always returns a valid newline-free JSON [`ApiResponse`] string: malformed
/// input yields an [`ApiResponse::error`] naming the parse failure, so a client
/// always gets a legible answer instead of a hang or dropped connection. (A bad
/// line has no recoverable `id`, so the error response carries none.)
#[must_use]
pub fn handle_line(line: &str, uptime_secs: u64) -> String {
    let resp = match serde_json::from_str::<ApiRequest>(line) {
        Ok(req) => dispatch(&req, uptime_secs),
        Err(e) => ApiResponse::error(format!("malformed request: {e}")),
    };
    serde_json::to_string(&resp).unwrap_or_else(|_| {
        r#"{"status":"error","message":"response serialization failed"}"#.to_owned()
    })
}

/// Serve the JSON-lines control protocol over TCP, blocking forever. One
/// request/response per connection (the `sdctl` model). `uptime` is a closure so
/// each request reports a live value. A single bad connection is logged and
/// skipped, never fatal to the listener.
///
/// # Errors
/// Returns the bind error if `addr` can't be listened on.
pub fn serve_tcp<A, F>(addr: A, uptime: F) -> std::io::Result<()>
where
    A: ToSocketAddrs,
    F: Fn() -> u64,
{
    let listener = TcpListener::bind(addr)?;
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                if let Err(e) = serve_one(s, &uptime) {
                    eprintln!("[attractor-api] tcp connection error: {e}");
                }
            }
            Err(e) => eprintln!("[attractor-api] tcp accept error: {e}"),
        }
    }
    Ok(())
}

/// Serve the control protocol over a Unix-domain socket at `path`, blocking
/// forever. Local-only transport; remote clients use [`serve_tcp`]. Only
/// compiled on Unix — Windows builds get TCP only, which is why the wire
/// contract lives in the portable [`attractor_proto`] crate.
///
/// # Errors
/// Returns the bind error if `path` can't be bound.
#[cfg(unix)]
pub fn serve_unix<P, F>(path: P, uptime: F) -> std::io::Result<()>
where
    P: AsRef<std::path::Path>,
    F: Fn() -> u64,
{
    use std::os::unix::net::UnixListener;
    let path = path.as_ref();
    // Clear a stale socket file from a previous run before binding.
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                if let Err(e) = serve_one(s, &uptime) {
                    eprintln!("[attractor-api] unix connection error: {e}");
                }
            }
            Err(e) => eprintln!("[attractor-api] unix accept error: {e}"),
        }
    }
    Ok(())
}

/// Read one request line from a connected stream, write one response line.
/// Generic over the stream type so TCP and Unix sockets share the path.
fn serve_one<S, F>(stream: S, uptime: &F) -> std::io::Result<()>
where
    S: std::io::Read + Write,
    F: Fn() -> u64,
{
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(()); // client closed without sending — nothing to answer
    }
    let resp = handle_line(line.trim_end(), uptime());
    let inner = reader.get_mut();
    inner.write_all(resp.as_bytes())?;
    inner.write_all(b"\n")?;
    inner.flush()
}

#[cfg(test)]
mod tests {
    use super::{dispatch, handle_line, resolve};
    use attractor_proto::{ApiRequest, ApiResponse, RequestKind};

    #[test]
    fn resolve_prefers_nonempty_override() {
        assert_eq!(resolve(Some("/custom.sock".into()), "/default.sock"), "/custom.sock");
    }

    #[test]
    fn resolve_falls_back_when_absent_or_blank() {
        assert_eq!(resolve(None, "/default.sock"), "/default.sock");
        assert_eq!(resolve(Some("   ".into()), "/default.sock"), "/default.sock");
    }

    #[test]
    fn dispatch_ping_is_ok_empty() {
        match dispatch(&ApiRequest::new(RequestKind::Ping), 0) {
            ApiResponse::Ok { data, .. } => assert!(data.is_null()),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_status_reports_uptime_no_store() {
        match dispatch(&ApiRequest::new(RequestKind::Status), 42) {
            ApiResponse::Ok { data, .. } => {
                assert_eq!(data["uptime_secs"], 42);
                assert_eq!(data["store_attached"], false);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_echoes_correlation_id() {
        let req = ApiRequest { id: Some(7), kind: RequestKind::Ping };
        match dispatch(&req, 0) {
            ApiResponse::Ok { id, .. } => assert_eq!(id, Some(7)),
            other => panic!("expected Ok with id, got {other:?}"),
        }
    }

    #[test]
    fn handle_line_round_trips_ping() {
        let out = handle_line(r#"{"kind":{"op":"ping"}}"#, 0);
        let resp: ApiResponse = serde_json::from_str(&out).unwrap();
        assert!(matches!(resp, ApiResponse::Ok { .. }));
    }

    #[test]
    fn handle_line_malformed_is_legible_error_not_panic() {
        let out = handle_line("not json at all", 0);
        match serde_json::from_str::<ApiResponse>(&out).unwrap() {
            ApiResponse::Error { message, .. } => {
                assert!(message.starts_with("malformed request:"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn handle_line_unknown_op_is_legible_error() {
        // A verb the proto doesn't define yet must parse-fail → legible Error.
        let out = handle_line(r#"{"kind":{"op":"teleport"}}"#, 0);
        assert!(matches!(
            serde_json::from_str::<ApiResponse>(&out).unwrap(),
            ApiResponse::Error { .. }
        ));
    }
}
