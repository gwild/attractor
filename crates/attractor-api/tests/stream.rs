//! Stream-level tests: the JSON-lines protocol over real TCP (and Unix
//! socket) connections, not just `handle_line` string dispatch.
//!
//! The serving loop runs in a background thread on a port-0 listener bound by
//! the test (`serve_tcp_on` / `serve_unix_on`); each request is its own
//! connection, matching the one-line-per-connection protocol. The serving
//! thread is detached and dies with the test process.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};

use attractor_api::serve_tcp_on;
use attractor_proto::ApiResponse;

/// Bind port 0, start the server thread, return the bound address.
fn spawn_tcp_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind port 0");
    let addr = listener.local_addr().expect("local_addr");
    std::thread::spawn(move || serve_tcp_on(listener, || 0));
    addr
}

/// One protocol round trip: connect, send one line, read one response line.
fn round_trip(addr: SocketAddr, line: &str) -> ApiResponse {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream.write_all(line.as_bytes()).expect("write");
    stream.write_all(b"\n").expect("write newline");
    stream.flush().expect("flush");
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    reader.read_line(&mut resp).expect("read response");
    serde_json::from_str(&resp).expect("response must be valid ApiResponse JSON")
}

#[test]
fn tcp_ping_round_trips() {
    let addr = spawn_tcp_server();
    let resp = round_trip(addr, r#"{"kind":{"op":"ping"}}"#);
    assert!(matches!(resp, ApiResponse::Ok { .. }));
}

#[test]
fn tcp_malformed_line_is_legible_error_not_dropped_connection() {
    let addr = spawn_tcp_server();
    match round_trip(addr, "not json at all") {
        ApiResponse::Error { message, .. } => {
            assert!(message.starts_with("malformed request:"));
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[test]
fn tcp_connections_are_independent_after_an_error() {
    // A malformed request on one connection must not poison the next.
    let addr = spawn_tcp_server();
    let _ = round_trip(addr, "garbage");
    let resp = round_trip(addr, r#"{"kind":{"op":"ping"}}"#);
    assert!(matches!(resp, ApiResponse::Ok { .. }));
}

#[cfg(not(feature = "store"))]
#[test]
fn tcp_matrix_verbs_fail_loud_without_store() {
    let addr = spawn_tcp_server();
    match round_trip(addr, r#"{"kind":{"op":"snapshot"}}"#) {
        ApiResponse::Error { message, .. } => {
            assert!(message.contains("store backend not attached"));
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[cfg(unix)]
mod unix_socket {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};

    #[test]
    fn unix_ping_round_trips() {
        let dir = std::env::temp_dir().join(format!("attractor-api-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("ctl.sock");
        let listener = UnixListener::bind(&path).expect("bind unix socket");
        std::thread::spawn(move || attractor_api::serve_unix_on(listener, || 0));

        let mut stream = UnixStream::connect(&path).expect("connect");
        stream.write_all(b"{\"kind\":{\"op\":\"ping\"}}\n").expect("write");
        stream.flush().expect("flush");
        let mut reader = BufReader::new(stream);
        let mut resp = String::new();
        reader.read_line(&mut resp).expect("read response");
        let resp: ApiResponse = serde_json::from_str(&resp).expect("valid ApiResponse");
        assert!(matches!(resp, ApiResponse::Ok { .. }));
    }
}

#[cfg(feature = "store")]
mod store_over_wire {
    use super::*;
    use attractor_core::{Cell, ContentHash, Primitive, SourceRef};
    use attractor_store::Node;

    fn insert_line(text: &str, origin: &str) -> String {
        let node = Node::Cell(Cell {
            value: Primitive::Text(text.to_owned()),
            source: SourceRef {
                content_hash: ContentHash::of(origin.as_bytes()),
                origin: origin.to_owned(),
            },
        });
        let node_json = serde_json::to_value(&node).expect("serialize node");
        serde_json::to_string(
            &serde_json::json!({"kind": {"op": "insert", "node": node_json}}),
        )
        .expect("serialize request")
    }

    fn data_of(resp: ApiResponse) -> serde_json::Value {
        match resp {
            ApiResponse::Ok { data, .. } => serde_json::to_value(data).expect("data to value"),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn convergence_holds_through_the_wire() {
        // The #2249 guarantee end-to-end over TCP: the same value inserted
        // from two origins on two separate connections is one node whose
        // provenance carries both, and the chain stays length 1.
        let addr = spawn_tcp_server();

        let a = data_of(round_trip(addr, &insert_line("fact", "#general")));
        let b = data_of(round_trip(addr, &insert_line("fact", "#issues")));
        let addr_a = a["address"].as_str().expect("insert returns address").to_owned();
        let addr_b = b["address"].as_str().expect("insert returns address").to_owned();
        assert_eq!(addr_a, addr_b, "same value must converge over the wire");

        let get = data_of(round_trip(
            addr,
            &serde_json::to_string(
                &serde_json::json!({"kind": {"op": "get", "address": addr_a}}),
            )
            .expect("serialize get"),
        ));
        let refs = get["provenance_refs"].as_array().expect("provenance_refs array");
        assert_eq!(refs.len(), 2, "both observations must be in provenance");

        let snap = data_of(round_trip(addr, r#"{"kind":{"op":"snapshot"}}"#));
        assert_eq!(snap["node_count"].as_u64(), Some(1));
        assert_eq!(snap["chain_len"].as_u64(), Some(1));
        assert_eq!(snap["chain_intact"].as_bool(), Some(true));
    }
}
