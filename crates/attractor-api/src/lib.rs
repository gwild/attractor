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
//! Build with the **`store`** feature for WS-1 matrix verbs (`Get`/`Insert`/`Link`/
//! `Snapshot`) backed by [`attractor_store::Store`] ([`dispatch_with_store`]).

use attractor_proto::{ApiRequest, ApiResponse, RequestKind};
use serde::Serialize;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, ToSocketAddrs};

#[cfg(feature = "store")]
use attractor_core::ContentHash;
#[cfg(feature = "store")]
use attractor_proto::{
    ContentHashHex, E2eReceipt, GetData, InsertData, LinkData, ProvenanceRef, SnapshotData,
};
#[cfg(feature = "store")]
use attractor_store::{Node, Store, StoreError};

/// Environment variable overriding the local Unix-socket path.
pub const ENV_SOCKET_PATH: &str = "ATTRACTOR_SOCKET_PATH";
/// Environment variable overriding the TCP bind/connect address.
pub const ENV_TCP_ADDR: &str = "ATTRACTOR_TCP_ADDR";

/// Resolve the control socket path from `$ATTRACTOR_SOCKET_PATH`, falling back
/// to the proto default.
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

#[must_use]
fn resolve(override_val: Option<String>, default: &str) -> String {
    match override_val {
        Some(v) if !v.trim().is_empty() => v,
        _ => default.to_owned(),
    }
}

/// Payload for a [`RequestKind::Status`] reply.
#[derive(Debug, Clone, Serialize)]
pub struct StatusData {
    /// Best-effort daemon uptime in seconds.
    pub uptime_secs: u64,
    /// Whether a store backend is attached for matrix verbs.
    pub store_attached: bool,
}

/// Pure request → response dispatch without a live store (portable 1.92 leg).
///
/// Answers `Ping`/`Status`; matrix verbs return a legible error unless the
/// **`store`** feature is enabled and [`dispatch_with_store`] is used instead.
#[must_use]
pub fn dispatch(req: &ApiRequest, uptime_secs: u64) -> ApiResponse {
    let resp = match &req.kind {
        RequestKind::Ping => ApiResponse::ok_empty(),
        RequestKind::Status => ApiResponse::ok(StatusData {
            uptime_secs,
            store_attached: cfg!(feature = "store"),
        }),
        RequestKind::Get { .. }
        | RequestKind::Insert { .. }
        | RequestKind::Link { .. }
        | RequestKind::Snapshot => {
            if cfg!(feature = "store") {
                ApiResponse::error(
                    "matrix verb requires dispatch_with_store — rebuild daemon with store feature",
                )
            } else {
                ApiResponse::error("store backend not attached")
            }
        }
        _ => ApiResponse::error("unsupported request verb for this daemon version"),
    };
    match req.id {
        Some(id) => resp.with_id(id),
        None => resp,
    }
}

#[cfg(feature = "store")]
fn hash_core(hex: &ContentHashHex) -> Result<ContentHash, String> {
    let s = hex.as_str();
    let mut digest = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        if i >= 32 {
            break;
        }
        let pair = std::str::from_utf8(chunk).map_err(|e| e.to_string())?;
        digest[i] = u8::from_str_radix(pair, 16).map_err(|e| e.to_string())?;
    }
    Ok(ContentHash::from_digest(digest))
}

#[cfg(feature = "store")]
fn hash_hex(hash: &ContentHash) -> ContentHashHex {
    ContentHashHex::new(hash.to_hex()).expect("core content hash hex is always valid")
}

#[cfg(feature = "store")]
fn store_error_message(err: StoreError) -> String {
    err.to_string()
}

#[cfg(feature = "store")]
fn provenance_wire(store: &Store, addr: &ContentHash) -> Vec<ProvenanceRef> {
    store
        .provenance(addr)
        .iter()
        .map(|source| ProvenanceRef {
            content_hash: hash_hex(&source.content_hash),
            origin: source.origin.clone(),
        })
        .collect()
}

#[cfg(feature = "store")]
fn receipt_for_get(address: ContentHashHex, provenance_refs: Vec<ProvenanceRef>) -> E2eReceipt {
    E2eReceipt {
        graph_address: address,
        source_spans: Vec::new(),
        transform_id: "attractor.store.get.v0".to_owned(),
        confidence: None,
        provenance_refs,
    }
}

/// Pure request → response dispatch against a live store (requires **`store`** feature).
#[cfg(feature = "store")]
#[must_use]
pub fn dispatch_with_store(req: &ApiRequest, uptime_secs: u64, store: &mut Store) -> ApiResponse {
    let resp = match &req.kind {
        RequestKind::Ping => ApiResponse::ok_empty(),
        RequestKind::Status => ApiResponse::ok(StatusData {
            uptime_secs,
            store_attached: true,
        }),
        RequestKind::Get { address } => match hash_core(address) {
            Ok(addr) => match store.get(&addr) {
                Some(node) => match serde_json::to_value(node) {
                    Ok(node_json) => {
                        let wire_addr = hash_hex(&addr);
                        let provenance_refs = provenance_wire(store, &addr);
                        let receipt = receipt_for_get(wire_addr.clone(), provenance_refs.clone());
                        ApiResponse::ok(GetData {
                            address: wire_addr,
                            node: node_json,
                            provenance_refs,
                            receipt,
                        })
                    }
                    Err(e) => ApiResponse::error(format!("node serialization failed: {e}")),
                },
                None => ApiResponse::error(format!(
                    "node {} is not in the store",
                    address.as_str()
                )),
            },
            Err(e) => ApiResponse::error(format!("invalid content address: {e}")),
        },
        RequestKind::Insert { node } => match serde_json::from_value::<Node>(node.clone()) {
            Ok(node) => match store.insert(node) {
                Ok(addr) => ApiResponse::ok(InsertData {
                    address: hash_hex(&addr),
                }),
                Err(e) => ApiResponse::error(store_error_message(e)),
            },
            Err(e) => ApiResponse::error(format!("invalid node payload: {e}")),
        },
        RequestKind::Link { from, to, kind } => match (hash_core(from), hash_core(to)) {
            (Ok(from_addr), Ok(to_addr)) => match store.link(from_addr, to_addr, kind.clone()) {
                Ok(edge) => ApiResponse::ok(LinkData {
                    from: hash_hex(&edge.from),
                    to: hash_hex(&edge.to),
                    kind: edge.kind,
                }),
                Err(e) => ApiResponse::error(store_error_message(e)),
            },
            (Err(e), _) | (_, Err(e)) => ApiResponse::error(format!("invalid content address: {e}")),
        },
        RequestKind::Snapshot => ApiResponse::ok(SnapshotData {
            node_count: store.len(),
            chain_len: store.chain_len(),
            head: store.head().map(|h| hash_hex(&h)),
            chain_intact: store.verify_chain(),
        }),
        _ => ApiResponse::error("unsupported request verb for this daemon version"),
    };
    match req.id {
        Some(id) => resp.with_id(id),
        None => resp,
    }
}

/// Parse one JSON request line, dispatch it, and serialize the response.
#[cfg(not(feature = "store"))]
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

/// Parse one JSON request line, dispatch against `store`, serialize the response.
#[cfg(feature = "store")]
#[must_use]
pub fn handle_line(line: &str, uptime_secs: u64, store: &mut Store) -> String {
    let resp = match serde_json::from_str::<ApiRequest>(line) {
        Ok(req) => dispatch_with_store(&req, uptime_secs, store),
        Err(e) => ApiResponse::error(format!("malformed request: {e}")),
    };
    serde_json::to_string(&resp).unwrap_or_else(|_| {
        r#"{"status":"error","message":"response serialization failed"}"#.to_owned()
    })
}

/// Serve the JSON-lines control protocol over TCP.
///
/// # Errors
/// Returns the bind error if `addr` can't be listened on.
#[cfg(not(feature = "store"))]
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

/// Serve the JSON-lines control protocol over TCP with an in-process store.
///
/// # Errors
/// Returns the bind error if `addr` can't be listened on.
#[cfg(feature = "store")]
pub fn serve_tcp<A, F>(addr: A, uptime: F) -> std::io::Result<()>
where
    A: ToSocketAddrs,
    F: Fn() -> u64,
{
    let listener = TcpListener::bind(addr)?;
    let mut store = Store::new();
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                if let Err(e) = serve_one(s, &uptime, &mut store) {
                    eprintln!("[attractor-api] tcp connection error: {e}");
                }
            }
            Err(e) => eprintln!("[attractor-api] tcp accept error: {e}"),
        }
    }
    Ok(())
}

/// Serve the control protocol over a Unix-domain socket.
///
/// # Errors
/// Returns the bind error if `path` can't be bound.
#[cfg(all(unix, not(feature = "store")))]
pub fn serve_unix<P, F>(path: P, uptime: F) -> std::io::Result<()>
where
    P: AsRef<std::path::Path>,
    F: Fn() -> u64,
{
    use std::os::unix::net::UnixListener;
    let path = path.as_ref();
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

/// Serve the control protocol over a Unix-domain socket with an in-process store.
///
/// # Errors
/// Returns the bind error if `path` can't be bound.
#[cfg(all(unix, feature = "store"))]
pub fn serve_unix<P, F>(path: P, uptime: F) -> std::io::Result<()>
where
    P: AsRef<std::path::Path>,
    F: Fn() -> u64,
{
    use std::os::unix::net::UnixListener;
    let path = path.as_ref();
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    let mut store = Store::new();
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                if let Err(e) = serve_one(s, &uptime, &mut store) {
                    eprintln!("[attractor-api] unix connection error: {e}");
                }
            }
            Err(e) => eprintln!("[attractor-api] unix accept error: {e}"),
        }
    }
    Ok(())
}

#[cfg(not(feature = "store"))]
fn serve_one<S, F>(stream: S, uptime: &F) -> std::io::Result<()>
where
    S: std::io::Read + Write,
    F: Fn() -> u64,
{
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    let resp = handle_line(line.trim_end(), uptime());
    let inner = reader.get_mut();
    inner.write_all(resp.as_bytes())?;
    inner.write_all(b"\n")?;
    inner.flush()
}

#[cfg(feature = "store")]
fn serve_one<S, F>(stream: S, uptime: &F, store: &mut Store) -> std::io::Result<()>
where
    S: std::io::Read + Write,
    F: Fn() -> u64,
{
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    let resp = handle_line(line.trim_end(), uptime(), store);
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
    fn dispatch_status_reports_uptime() {
        match dispatch(&ApiRequest::new(RequestKind::Status), 42) {
            ApiResponse::Ok { data, .. } => {
                assert_eq!(data["uptime_secs"], 42);
                assert_eq!(data["store_attached"], cfg!(feature = "store"));
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
        #[cfg(not(feature = "store"))]
        let out = handle_line(r#"{"kind":{"op":"ping"}}"#, 0);
        #[cfg(feature = "store")]
        let out = {
            use attractor_store::Store;
            handle_line(r#"{"kind":{"op":"ping"}}"#, 0, &mut Store::new())
        };
        let resp: ApiResponse = serde_json::from_str(&out).unwrap();
        assert!(matches!(resp, ApiResponse::Ok { .. }));
    }

    #[test]
    fn handle_line_malformed_is_legible_error_not_panic() {
        #[cfg(not(feature = "store"))]
        let out = handle_line("not json at all", 0);
        #[cfg(feature = "store")]
        let out = {
            use attractor_store::Store;
            handle_line("not json at all", 0, &mut Store::new())
        };
        match serde_json::from_str::<ApiResponse>(&out).unwrap() {
            ApiResponse::Error { message, .. } => {
                assert!(message.starts_with("malformed request:"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn handle_line_unknown_op_is_legible_error() {
        #[cfg(not(feature = "store"))]
        let out = handle_line(r#"{"kind":{"op":"teleport"}}"#, 0);
        #[cfg(feature = "store")]
        let out = {
            use attractor_store::Store;
            handle_line(r#"{"kind":{"op":"teleport"}}"#, 0, &mut Store::new())
        };
        assert!(matches!(
            serde_json::from_str::<ApiResponse>(&out).unwrap(),
            ApiResponse::Error { .. }
        ));
    }

    #[cfg(feature = "store")]
    mod store_tests {
        use crate::dispatch_with_store;
        use crate::dispatch;
        use attractor_core::{Cell, ContentHash, Primitive, SourceRef};
        use attractor_proto::{ApiRequest, ApiResponse, RequestKind};
        use attractor_store::{Node, Store};

        fn sample_cell(text: &str) -> Node {
            Node::Cell(Cell {
                value: Primitive::Text(text.to_owned()),
                source: SourceRef {
                    content_hash: ContentHash::of(b"origin"),
                    origin: "#test".to_owned(),
                },
            })
        }

        #[test]
        fn insert_get_round_trip() {
            let mut store = Store::new();
            let cell = sample_cell("hello");
            let addr = store.insert(cell.clone()).unwrap();
            let hex = addr.to_hex();
            let get_req = ApiRequest::new(RequestKind::Get {
                address: attractor_proto::ContentHashHex::new(hex).unwrap(),
            });
            match dispatch_with_store(&get_req, 0, &mut store) {
                ApiResponse::Ok { data, .. } => {
                    assert_eq!(
                        data["receipt"]["transform_id"],
                        "attractor.store.get.v0",
                        "full data: {data}"
                    );
                    assert!(data["node"].is_object(), "full data: {data}");
                    assert!(data["address"].is_string(), "full data: {data}");
                }
                other => panic!("expected Ok, got {other:?}"),
            }
        }

        #[test]
        fn snapshot_reports_store_state() {
            let mut store = Store::new();
            store.insert(sample_cell("x")).unwrap();
            match dispatch_with_store(&ApiRequest::new(RequestKind::Snapshot), 0, &mut store) {
                ApiResponse::Ok { data, .. } => {
                    assert_eq!(data["node_count"], 1);
                    assert_eq!(data["chain_intact"], true);
                }
                other => panic!("expected Ok, got {other:?}"),
            }
        }

        #[test]
        fn dispatch_without_store_instance_errors_matrix_verbs() {
            match dispatch(
                &ApiRequest::new(RequestKind::Snapshot),
                0,
            ) {
                ApiResponse::Error { message, .. } => {
                    assert!(message.contains("dispatch_with_store"));
                }
                other => panic!("expected Error, got {other:?}"),
            }
        }
    }
}
