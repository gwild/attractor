//! Portable wire types for the Attractor control plane (#2243).
//!
//! Extracted as a standalone crate (the proto-split blessed in the #2243 dedup
//! thread) so the request/response contract stays **cross-platform attestable**
//! — pure `serde`, no OS/async/socket dependencies. The Linux-only transport
//! (`UnixStream`, TCP listener) lives in `attractor-api`; this crate is the
//! shared contract that both the server and any client depend on.
//!
//! Adapted from stringdriver's `terminal_api` envelope shape (the reusable
//! skeleton — its stepper/audio domain verbs are intentionally *not* carried
//! over). v0 ships only the universal verbs (`Ping`, `Status`); the matrix-op
//! verbs (`Get`/`Insert`/`Link`/`Snapshot`) map onto `attractor-store`'s API
//! and are added there once it lands — so [`RequestKind`] is
//! `#[non_exhaustive]` to admit them without breaking the envelope.

use serde::{Deserialize, Serialize};

/// Default TCP port for the control plane — the portable transport.
/// (`SD` → `7533` on a phone keypad, inherited from stringdriver.)
pub const TCP_PORT: u16 = 7533;

/// Default Unix-domain socket path — the Linux/Unix local fast path
/// (server-side only; clients on other platforms use [`Transport::Tcp`]).
pub const SOCKET_PATH: &str = "/tmp/attractor_api.sock";

/// Which transport a client/server is using. `Tcp` is portable and works on
/// every platform; `UnixSocket` is the Linux/Unix local fast path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    UnixSocket,
    Tcp,
}

/// A control-plane request envelope. `id` lets a client correlate the matching
/// [`ApiResponse`]; `kind` carries the verb.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiRequest {
    /// Optional client-assigned correlation id, echoed back in the response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub kind: RequestKind,
}

impl ApiRequest {
    /// Convenience constructor for an un-correlated request.
    pub fn new(kind: RequestKind) -> Self {
        Self { id: None, kind }
    }
}

/// Request verbs. v0 ships only the universal verbs; domain/matrix ops are
/// added by the `attractor-api`/`attractor-store` owners — hence
/// `#[non_exhaustive]`, so adding a variant is not a breaking change here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RequestKind {
    /// Connectivity/liveness check. Server replies `Ok` with empty data.
    Ping,
    /// Service status snapshot. Payload shape is defined by the server.
    Status,
}

/// A control-plane response envelope, serde-tagged on `status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ApiResponse {
    Ok {
        /// Echoes the request's correlation id when one was supplied.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<u64>,
        /// Arbitrary JSON payload (the verb decides its shape).
        data: serde_json::Value,
    },
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<u64>,
        message: String,
    },
}

impl ApiResponse {
    /// Build an `Ok` response from any serializable payload.
    /// Falls back to `null` if the payload fails to serialize.
    pub fn ok(data: impl Serialize) -> Self {
        let data = serde_json::to_value(data).unwrap_or(serde_json::Value::Null);
        ApiResponse::Ok { id: None, data }
    }

    /// Build an `Ok` response carrying no payload.
    pub fn ok_empty() -> Self {
        ApiResponse::Ok {
            id: None,
            data: serde_json::Value::Null,
        }
    }

    /// Build an `Error` response.
    pub fn error(message: impl Into<String>) -> Self {
        ApiResponse::Error {
            id: None,
            message: message.into(),
        }
    }

    /// Attach/override the correlation id (so a server can echo the request's).
    pub fn with_id(mut self, new_id: u64) -> Self {
        match &mut self {
            ApiResponse::Ok { id, .. } | ApiResponse::Error { id, .. } => *id = Some(new_id),
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips() {
        let req = ApiRequest::new(RequestKind::Ping);
        let json = serde_json::to_string(&req).unwrap();
        let back: ApiRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn request_id_omitted_when_absent_present_when_set() {
        assert_eq!(
            serde_json::to_string(&ApiRequest::new(RequestKind::Status)).unwrap(),
            r#"{"kind":{"op":"status"}}"#
        );
        let with = ApiRequest { id: Some(9), kind: RequestKind::Ping };
        assert_eq!(
            serde_json::to_string(&with).unwrap(),
            r#"{"id":9,"kind":{"op":"ping"}}"#
        );
    }

    #[test]
    fn verbs_tagged_by_op() {
        assert_eq!(serde_json::to_string(&RequestKind::Ping).unwrap(), r#"{"op":"ping"}"#);
        assert_eq!(serde_json::to_string(&RequestKind::Status).unwrap(), r#"{"op":"status"}"#);
    }

    #[test]
    fn ok_wraps_payload() {
        match ApiResponse::ok(serde_json::json!({"v": 1})) {
            ApiResponse::Ok { data, .. } => assert_eq!(data, serde_json::json!({"v": 1})),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn error_carries_message() {
        match ApiResponse::error("nope") {
            ApiResponse::Error { message, .. } => assert_eq!(message, "nope"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn response_with_id_echoes() {
        let r = ApiResponse::ok_empty().with_id(42);
        match r {
            ApiResponse::Ok { id, .. } => assert_eq!(id, Some(42)),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn response_round_trips() {
        for r in [ApiResponse::ok_empty(), ApiResponse::error("x").with_id(3)] {
            let json = serde_json::to_string(&r).unwrap();
            let back: ApiResponse = serde_json::from_str(&json).unwrap();
            assert_eq!(r, back);
        }
    }
}
