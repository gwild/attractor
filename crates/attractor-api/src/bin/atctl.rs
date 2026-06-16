//! `atctl` — terminal control client for the Attractor daemon (the `sdctl`
//! analog). Sends one [`ApiRequest`] and prints the daemon's JSON response.
//!
//! Usage:
//! ```text
//!   atctl ping                          # liveness, over the local Unix socket
//!   atctl status                        # status snapshot
//!   atctl snapshot                      # store integrity snapshot (store feature)
//!   atctl get <ADDR>                    # fetch a node + provenance by address
//!   atctl insert <JSON|->               # insert a Cell/Frame node (JSON arg, or - for stdin)
//!   atctl link <FROM> <TO> <KIND>       # add a typed edge between two nodes
//!   atctl relations <ADDR>              # read incident edges — the recall traversal
//!   atctl --tcp 192.168.1.55:7533 ping  # remote, over TCP
//! ```
//! Transport defaults to the Unix socket ([`attractor_proto::SOCKET_PATH`]) on
//! Unix; `--tcp host:port` selects TCP (the only option off-Unix).
//!
//! The matrix/recall verbs (`get`/`insert`/`link`/`snapshot`/`relations`) map
//! onto the store-backed daemon dispatch; against a daemon built without the
//! `store` feature they return a legible "unsupported request verb" error.

use attractor_proto::{ApiRequest, ContentHashHex, RequestKind, SOCKET_PATH, TCP_PORT};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::ExitCode;

fn usage() -> ExitCode {
    eprintln!(
        "usage: atctl [--tcp HOST:PORT] <command>\n\
         \n\
         commands:\n\
         \x20 ping                       liveness check\n\
         \x20 status                     service status snapshot\n\
         \x20 snapshot                   store integrity snapshot (store feature)\n\
         \x20 get <ADDR>                 fetch a node + provenance by content address\n\
         \x20 insert <JSON|->            insert a Cell/Frame node (JSON arg, or - to read stdin)\n\
         \x20 link <FROM> <TO> <KIND>    add a typed edge between two stored nodes\n\
         \x20 relations <ADDR>           read edges incident to a node (recall traversal)\n\
         \n\
         default transport: unix socket {SOCKET_PATH} (TCP port {TCP_PORT} for remote)"
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut tcp: Option<String> = None;
    // Positional args after flags: the command and its operands.
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tcp" => {
                let Some(addr) = args.get(i + 1) else {
                    return usage();
                };
                tcp = Some(addr.clone());
                i += 2;
            }
            "-h" | "--help" => return usage(),
            other => {
                positional.push(other.to_owned());
                i += 1;
            }
        }
    }

    let Some(cmd) = positional.first() else {
        return usage();
    };
    let operands = &positional[1..];

    // Build the request verb. Operand parsing fails loud (legible message +
    // exit 2) rather than sending a malformed request — a recall/insert against
    // a bogus address or missing operand must never look like a daemon error.
    let kind = match cmd.as_str() {
        "ping" => RequestKind::Ping,
        "status" => RequestKind::Status,
        "snapshot" => RequestKind::Snapshot,
        "get" => match parse_addr(operands, "get <ADDR>") {
            Ok(address) => RequestKind::Get { address },
            Err(code) => return code,
        },
        "relations" => match parse_addr(operands, "relations <ADDR>") {
            Ok(address) => RequestKind::Relations { address },
            Err(code) => return code,
        },
        "insert" => match parse_insert(operands) {
            Ok(node) => RequestKind::Insert { node },
            Err(code) => return code,
        },
        "link" => match parse_link(operands) {
            Ok(kind) => kind,
            Err(code) => return code,
        },
        _ => return usage(),
    };

    match send(&ApiRequest::new(kind), tcp.as_deref()) {
        Ok(response_line) => {
            println!("{response_line}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("atctl: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Parse exactly one content-address operand, validating the hash hex.
fn parse_addr(operands: &[String], form: &str) -> Result<ContentHashHex, ExitCode> {
    let [addr] = operands else {
        eprintln!("atctl: usage: atctl {form}");
        return Err(ExitCode::from(2));
    };
    ContentHashHex::new(addr.clone()).map_err(|e| {
        eprintln!("atctl: invalid content address {addr:?}: {e}");
        ExitCode::from(2)
    })
}

/// Parse the insert node JSON, from a single argument or stdin (`-`). Validates
/// it is well-formed JSON before sending; the daemon owns Cell/Frame admission.
fn parse_insert(operands: &[String]) -> Result<serde_json::Value, ExitCode> {
    let [src] = operands else {
        eprintln!("atctl: usage: atctl insert <JSON|->  (- reads node JSON from stdin)");
        return Err(ExitCode::from(2));
    };
    let raw = if src == "-" {
        let mut buf = String::new();
        if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
            eprintln!("atctl: failed reading node JSON from stdin: {e}");
            return Err(ExitCode::FAILURE);
        }
        buf
    } else {
        src.clone()
    };
    serde_json::from_str::<serde_json::Value>(&raw).map_err(|e| {
        eprintln!("atctl: insert node is not valid JSON: {e}");
        ExitCode::from(2)
    })
}

/// Parse `link <FROM> <TO> <KIND>`, validating both addresses and a non-empty kind.
fn parse_link(operands: &[String]) -> Result<RequestKind, ExitCode> {
    let [from, to, kind] = operands else {
        eprintln!("atctl: usage: atctl link <FROM> <TO> <KIND>");
        return Err(ExitCode::from(2));
    };
    if kind.trim().is_empty() {
        eprintln!("atctl: link <KIND> may not be empty");
        return Err(ExitCode::from(2));
    }
    let from = ContentHashHex::new(from.clone()).map_err(|e| {
        eprintln!("atctl: invalid FROM address {from:?}: {e}");
        ExitCode::from(2)
    })?;
    let to = ContentHashHex::new(to.clone()).map_err(|e| {
        eprintln!("atctl: invalid TO address {to:?}: {e}");
        ExitCode::from(2)
    })?;
    Ok(RequestKind::Link {
        from,
        to,
        kind: kind.clone(),
    })
}

/// Connect (TCP if `tcp_addr` is set, else the Unix socket), send one request
/// line, and return the response line.
fn send(request: &ApiRequest, tcp_addr: Option<&str>) -> std::io::Result<String> {
    let line = serde_json::to_string(request).map_err(std::io::Error::other)?;
    if let Some(addr) = tcp_addr {
        let stream = std::net::TcpStream::connect(addr)?;
        exchange(stream, &line)
    } else {
        connect_local(&line)
    }
}

#[cfg(unix)]
fn connect_local(line: &str) -> std::io::Result<String> {
    // Env-aware (ATTRACTOR_SOCKET_PATH) so client + daemon resolve the same
    // path from config, not a hard-coded constant.
    let stream = std::os::unix::net::UnixStream::connect(attractor_api::socket_path())?;
    exchange(stream, line)
}

#[cfg(not(unix))]
fn connect_local(_line: &str) -> std::io::Result<String> {
    // No Unix sockets off-Unix: target the daemon over TCP with `--tcp`.
    Err(std::io::Error::other(
        "no local Unix socket on this platform — use --tcp HOST:PORT",
    ))
}

/// Write one request line, read one response line.
fn exchange<S: std::io::Read + Write>(mut stream: S, line: &str) -> std::io::Result<String> {
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    reader.read_line(&mut resp)?;
    Ok(resp.trim_end().to_owned())
}
