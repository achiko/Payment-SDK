use indexing::SourceError;

use super::transport::{Error, Failure};

#[derive(Debug)]
pub(super) struct CallFailure {
    pub(super) remote_code: Option<i64>,
    pub(super) error: SourceError,
}

impl CallFailure {
    pub(super) fn remote(failure: Failure) -> Self {
        let retryable = failure.code == -28 || failure.is_server_error();
        Self {
            remote_code: Some(failure.code),
            error: source_error(
                format!("Bitcoin JSON-RPC request failed with code {}", failure.code),
                retryable,
            ),
        }
    }

    pub(super) fn local(error: SourceError) -> Self {
        Self {
            remote_code: None,
            error,
        }
    }
}

// design-lint: allow unclassified-free-function -- Bitcoin RPC translation between foreign JSON-RPC and indexing errors preserves source text and retryability without adding chain policy to either generic crate
pub(super) fn map_json_rpc_error(error: Error) -> SourceError {
    source_error(error.to_string(), error.is_retryable())
}

pub(crate) fn source_error(message: impl Into<String>, retryable: bool) -> SourceError {
    SourceError {
        message: message.into(),
        retryable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use json_rpc::ErrorKind;

    #[test]
    fn local_rpc_errors_preserve_message_and_transport_retryability() {
        for (kind, retryable) in [
            (ErrorKind::InvalidConfiguration, false),
            (ErrorKind::InvalidRequest, false),
            (ErrorKind::Timeout, true),
            (ErrorKind::Unavailable, true),
            (ErrorKind::HttpStatus(429), true),
            (ErrorKind::HttpStatus(502), true),
            (ErrorKind::HttpStatus(503), true),
            (ErrorKind::HttpStatus(504), true),
            (ErrorKind::HttpStatus(400), false),
            (ErrorKind::ResponseTooLarge, false),
            (ErrorKind::InvalidResponse, false),
        ] {
            let message = "transport context\nkept verbatim";
            let mapped = map_json_rpc_error(Error {
                kind,
                message: message.to_owned(),
            });
            assert_eq!(mapped.message, message);
            assert_eq!(mapped.retryable, retryable);
        }
    }

    #[test]
    fn remote_failures_keep_code_and_core_retry_policy_without_provider_payload() {
        for (code, retryable) in [
            (-28, true),
            (-26, false),
            (-32_099, true),
            (-32_000, true),
            (-32_100, false),
            (-31_999, false),
            (i64::MIN, false),
            (i64::MAX, false),
        ] {
            let mapped = CallFailure::remote(Failure {
                code,
                message: "provider-only message".to_owned(),
                data: Some(json_rpc::RawJson::from_serializable(&"provider-only data").unwrap()),
            });
            assert_eq!(mapped.remote_code, Some(code));
            assert_eq!(
                mapped.error.message,
                format!("Bitcoin JSON-RPC request failed with code {code}")
            );
            assert_eq!(mapped.error.retryable, retryable);
        }
    }
}
