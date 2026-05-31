//! `atctl` — terminal control client for the Attractor daemon (the `sdctl`
//! analog). Sends one [`ApiRequest`] and prints the daemon's JSON response.
//!
//! Usage:
//! ```text
//!   atctl ping                          # liveness, over the local Unix socket
//!   atctl status                        # status snapshot
//!   atctl --tcp 192.168.1.55:7533 ping  # remote, over TCP
//! ```
//! Transport defaults to the Unix socket ([`attractor_proto::SOCKET_PATH`]) on
//! Unix; `--tcp host:port` selects TCP (the only option off-Unix).

use attractor_proto::{ApiRequest, RequestKind, SOCKET_PATH, TCP_PORT};
use std::io::{BufRead, BufReader, Write};
use std::process::ExitCode;

fn usage() -> ExitCode {
    eprintln!(
        "usage: atctl [--tcp HOST:PORT] <ping|status>\n\
         \n\
         default transport: unix socket {SOCKET_PATH} (TCP port {TCP_PORT} for remote)"
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut tcp: Option<String> = None;
    let mut cmd: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tcp" => {
                let Some(addr) = args.get(i + 1) else { return usage() };
                tcp = Some(addr.clone());
                i += 2;
            }
            "-h" | "--help" => return usage(),
            other if cmd.is_none() => {
                cmd = Some(other.to_owned());
                i += 1;
            }
            _ => return usage(),
        }
    }

    let Some(cmd) = cmd else { return usage() };
    let kind = match cmd.as_str() {
        "ping" => RequestKind::Ping,
        "status" => RequestKind::Status,
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
    let stream = std::os::unix::net::UnixStream::connect(SOCKET_PATH)?;
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
