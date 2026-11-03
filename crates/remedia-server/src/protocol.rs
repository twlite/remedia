use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u16 = 1;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RpcRequest {
    pub(crate) version: u16,
    pub(crate) id: String,
    pub(crate) command: String,
    #[serde(default = "empty_params")]
    pub(crate) params: Value,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct RpcResponse {
    pub(crate) version: u16,
    pub(crate) id: Option<String>,
    #[serde(flatten)]
    pub(crate) result: ResponseResult,
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum ResponseResult {
    Success { ok: bool, result: Value },
    Failure { ok: bool, error: RpcError },
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct RpcError {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

impl RpcError {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl RpcResponse {
    pub(crate) fn success(id: Option<String>, result: Value) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id,
            result: ResponseResult::Success { ok: true, result },
        }
    }

    pub(crate) fn failure(
        id: Option<String>,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id,
            result: ResponseResult::Failure {
                ok: false,
                error: RpcError {
                    code,
                    message: message.into(),
                },
            },
        }
    }
}

pub(crate) fn parse_request(payload: &str) -> Result<RpcRequest, RpcError> {
    let request: RpcRequest = serde_json::from_str(payload).map_err(|_| RpcError {
        code: "invalid_request",
        message: "message is not a valid RPC request".into(),
    })?;
    if request.version != PROTOCOL_VERSION {
        return Err(RpcError {
            code: "invalid_request",
            message: format!(
                "unsupported protocol version {}; expected {PROTOCOL_VERSION}",
                request.version
            ),
        });
    }
    if request.id.is_empty() || request.id.len() > 128 {
        return Err(RpcError {
            code: "invalid_request",
            message: "request id must contain 1 to 128 bytes".into(),
        });
    }
    if request.command.is_empty() || request.command.len() > 128 {
        return Err(RpcError {
            code: "invalid_request",
            message: "command must contain 1 to 128 bytes".into(),
        });
    }
    if !request.params.is_object() {
        return Err(RpcError {
            code: "invalid_request",
            message: "params must be a JSON object".into(),
        });
    }
    Ok(request)
}

pub(crate) fn event(name: &str, session_id: &str, sequence: u64, data: Value) -> Value {
    serde_json::json!({
        "version": PROTOCOL_VERSION,
        "event": name,
        "sessionId": session_id,
        "sequence": sequence,
        "data": data,
    })
}

fn empty_params() -> Value {
    Value::Object(Default::default())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{PROTOCOL_VERSION, RpcResponse, parse_request};

    #[test]
    fn validates_version_and_request_correlation_fields() {
        let request =
            parse_request(r#"{"version":1,"id":"r1","command":"node.info","params":{}}"#).unwrap();
        assert_eq!(request.version, PROTOCOL_VERSION);
        assert_eq!(request.id, "r1");
        assert!(
            parse_request(r#"{"version":2,"id":"r1","command":"node.info","params":{}}"#).is_err()
        );
        assert!(
            parse_request(r#"{"version":1,"id":"","command":"node.info","params":{}}"#).is_err()
        );
        assert!(
            parse_request(r#"{"version":1,"id":"r1","command":"node.info","params":{},"extra":1}"#)
                .is_err()
        );
    }

    #[test]
    fn success_and_failure_responses_keep_the_same_request_id() {
        let success =
            serde_json::to_value(RpcResponse::success(Some("r2".into()), json!({"ok":true})))
                .unwrap();
        assert_eq!(success["id"], "r2");
        assert_eq!(success["ok"], true);
        let failure = serde_json::to_value(RpcResponse::failure(
            Some("r3".into()),
            "invalid_state",
            "not ready",
        ))
        .unwrap();
        assert_eq!(failure["id"], "r3");
        assert_eq!(failure["error"]["code"], "invalid_state");
    }
}
