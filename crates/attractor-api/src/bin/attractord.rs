//! `attractord` — the Attractor daemon (the `atctl` counterpart). Serves the
//! JSON-lines control protocol on ONE transport per process.
//!
//! Usage:
//! ```text
//!   attractord                       # Unix socket (ATTRACTOR_SOCKET_PATH or proto default)
//!   attractord --tcp 127.0.0.1:7533  # TCP listener instead (the only option off-Unix)
//! ```
//!
//! One transport per process is deliberate, not a shortcut: with the `store`
//! feature each serving loop owns its own in-process [`attractor_store::Store`],
//! so a second concurrent transport would mean a second, silently divergent
//! store. When a shared store lands (post projector-boundary), this constraint
//! lifts and a `--tcp`-alongside-socket mode can ride that change.
//!
//! Transport resolution mirrors `atctl`: env-aware socket path via
//! [`attractor_api::socket_path`], TCP only when `--tcp` names an address
//! explicitly — the daemon never falls back to the `0.0.0.0` default of
//! [`attractor_api::tcp_addr`], so an operator cannot end up with an
//! all-interfaces listener without writing the address out.

use std::process::ExitCode;
use std::time::Instant;

fn usage() -> ExitCode {
    eprintln!(
        "usage: attractord [--tcp HOST:PORT]\n\
         \n\
         default transport: unix socket (ATTRACTOR_SOCKET_PATH or {})\n\
         --tcp requires an explicit bind address; there is no implicit 0.0.0.0",
        attractor_proto::SOCKET_PATH
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut tcp: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tcp" => {
                let Some(addr) = args.get(i + 1) else { return usage() };
                tcp = Some(addr.clone());
                i += 2;
            }
            "-h" | "--help" => return usage(),
            _ => return usage(),
        }
    }

    let start = Instant::now();
    let uptime = move || start.elapsed().as_secs();

    let served = match tcp {
        Some(addr) => {
            eprintln!("[attractord] serving tcp {addr}");
            attractor_api::serve_tcp(addr, uptime)
        }
        None => serve_local(uptime),
    };

    match served {
        // The accept loops only return by error; a clean return still means
        // the daemon stopped serving, which systemd should treat as failure.
        Ok(()) => {
            eprintln!("[attractord] accept loop ended");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("attractord: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(unix)]
fn serve_local<F>(uptime: F) -> std::io::Result<()>
where
    F: Fn() -> u64,
{
    let path = attractor_api::socket_path();
    eprintln!("[attractord] serving unix socket {path}");
    attractor_api::serve_unix(path, uptime)
}

#[cfg(not(unix))]
fn serve_local<F>(_uptime: F) -> std::io::Result<()>
where
    F: Fn() -> u64,
{
    // No Unix sockets off-Unix: require an explicit TCP bind instead.
    Err(std::io::Error::other(
        "no local Unix socket on this platform — use --tcp HOST:PORT",
    ))
}
