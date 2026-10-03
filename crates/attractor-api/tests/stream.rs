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
        stream
            .write_all(b"{\"kind\":{\"op\":\"ping\"}}\n")
            .expect("write");
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
    use attractor_proto::ContentHashHex;
    use attractor_store::Node;
    use std::path::PathBuf;

    fn spawn_tcp_server_persistent(path: PathBuf) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind port 0");
        let addr = listener.local_addr().expect("local_addr");
        std::thread::spawn(move || attractor_api::serve_tcp_on_persistent(listener, || 0, path));
        addr
    }

    fn insert_line(text: &str, origin: &str) -> String {
        let node = Node::Cell(Cell {
            value: Primitive::Text(text.to_owned()),
            source: SourceRef {
                content_hash: ContentHash::of(origin.as_bytes()),
                origin: origin.to_owned(),
            },
        });
        let node_json = serde_json::to_value(&node).expect("serialize node");
        serde_json::to_string(&serde_json::json!({"kind": {"op": "insert", "node": node_json}}))
            .expect("serialize request")
    }

    fn data_of(resp: ApiResponse) -> serde_json::Value {
        match resp {
            ApiResponse::Ok { data, .. } => serde_json::to_value(data).expect("data to value"),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    fn get_line(address: &str) -> String {
        serde_json::to_string(&serde_json::json!({"kind": {"op": "get", "address": address}}))
            .expect("serialize get")
    }

    fn link_line(from: &str, to: &str, kind: &str) -> String {
        serde_json::to_string(
            &serde_json::json!({"kind": {"op": "link", "from": from, "to": to, "kind": kind}}),
        )
        .expect("serialize link")
    }

    fn relations_line(address: &str) -> String {
        serde_json::to_string(&serde_json::json!({"kind": {"op": "relations", "address": address}}))
            .expect("serialize relations")
    }

    fn weighted_relations_line(address: &str) -> String {
        serde_json::to_string(
            &serde_json::json!({"kind": {"op": "weighted_relations", "address": address}}),
        )
        .expect("serialize weighted relations")
    }

    #[test]
    fn convergence_holds_through_the_wire() {
        // The #2249 guarantee end-to-end over TCP: the same value inserted
        // from two origins on two separate connections is one node whose
        // provenance carries both, and the chain stays length 1.
        let addr = spawn_tcp_server();

        let a = data_of(round_trip(addr, &insert_line("fact", "#general")));
        let b = data_of(round_trip(addr, &insert_line("fact", "#issues")));
        let addr_a = a["address"]
            .as_str()
            .expect("insert returns address")
            .to_owned();
        let addr_b = b["address"]
            .as_str()
            .expect("insert returns address")
            .to_owned();
        assert_eq!(addr_a, addr_b, "same value must converge over the wire");

        let get = data_of(round_trip(addr, &get_line(&addr_a)));
        let refs = get["provenance_refs"]
            .as_array()
            .expect("provenance_refs array");
        assert_eq!(refs.len(), 2, "both observations must be in provenance");

        let snap = data_of(round_trip(addr, r#"{"kind":{"op":"snapshot"}}"#));
        assert_eq!(snap["node_count"].as_u64(), Some(1));
        assert_eq!(snap["chain_len"].as_u64(), Some(1));
        assert_eq!(snap["chain_intact"].as_bool(), Some(true));
    }

    #[test]
    fn persistent_store_survives_new_server_instance() {
        let dir =
            std::env::temp_dir().join(format!("attractor-api-persist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("store.json");

        let first = spawn_tcp_server_persistent(path.clone());
        let a = data_of(round_trip(first, &insert_line("persist-a", "#general")));
        let b = data_of(round_trip(first, &insert_line("persist-b", "#general")));
        let addr_a = a["address"]
            .as_str()
            .expect("insert returns address")
            .to_owned();
        let addr_b = b["address"]
            .as_str()
            .expect("insert returns address")
            .to_owned();
        ContentHashHex::new(addr_a.clone()).expect("address is valid hex");
        ContentHashHex::new(addr_b.clone()).expect("address is valid hex");
        let _ = data_of(round_trip(
            first,
            &link_line(&addr_a, &addr_b, "references"),
        ));

        let second = spawn_tcp_server_persistent(path.clone());
        let snap = data_of(round_trip(second, r#"{"kind":{"op":"snapshot"}}"#));
        assert_eq!(snap["node_count"].as_u64(), Some(2));
        assert_eq!(snap["chain_len"].as_u64(), Some(2));
        assert_eq!(snap["chain_intact"].as_bool(), Some(true));

        let get = data_of(round_trip(second, &get_line(&addr_a)));
        assert_eq!(get["address"].as_str(), Some(addr_a.as_str()));
        let rel = data_of(round_trip(second, &relations_line(&addr_a)));
        assert_eq!(rel["edges"].as_array().expect("edges array").len(), 1);

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn weighted_relations_cross_the_wire() {
        let addr = spawn_tcp_server();
        let a = data_of(round_trip(addr, &insert_line("edge-a", "#general")));
        let b = data_of(round_trip(addr, &insert_line("edge-b", "#general")));
        let c = data_of(round_trip(addr, &insert_line("edge-c", "#general")));
        let addr_a = a["address"].as_str().expect("address").to_owned();
        let addr_b = b["address"].as_str().expect("address").to_owned();
        let addr_c = c["address"].as_str().expect("address").to_owned();
        let _ = data_of(round_trip(addr, &link_line(&addr_a, &addr_b, "defines")));
        let _ = data_of(round_trip(
            addr,
            &link_line(&addr_c, &addr_b, "contradicts"),
        ));

        let rel = data_of(round_trip(addr, &weighted_relations_line(&addr_b)));
        let edges = rel["edges"].as_array().expect("edges array");
        assert_eq!(edges.len(), 2);
        assert!(edges.iter().all(|edge| edge["to"] == addr_b));
        assert!(edges.iter().all(|edge| edge["to_heat"]["heat"] == 1.0));
        assert!(edges.iter().all(|edge| edge["from_heat"]["heat"] == 0.5));
        assert!(edges.iter().all(|edge| edge["weight"] == 0.75));
    }
}
