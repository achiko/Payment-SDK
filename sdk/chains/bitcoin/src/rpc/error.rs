use indexing::SourceError;

use super::transport::{Error, Failure};

#[derive(Debug)]
pub(super) struct CallFailure {
    pub(super) remote_code: Option<i64>,
    pub(super) error: SourceError,
}

impl CallFailure {
    pub(super) fn local(error: SourceError) -> Self {
        Self {
            remote_code: None,
            error,
        }
    }
}

pub(super) fn map_json_rpc_error(error: Error) -> SourceError {
    source_error(error.to_string(), error.is_retryable())
}

pub(super) fn map_remote_failure(failure: Failure) -> SourceError {
    let retryable = failure.code == -28 || failure.is_server_error();
    source_error(
        format!("Bitcoin JSON-RPC request failed with code {}", failure.code),
        retryable,
    )
}

pub(crate) fn source_error(message: impl Into<String>, retryable: bool) -> SourceError {
    SourceError {
        message: message.into(),
        retryable,
    }
}
