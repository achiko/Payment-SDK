//! Replaceable mock transport contracts and adapters.

/// Delivers an already-formed RPC request.
pub trait Transport {
    const NAME: &'static str;

    fn endpoint(&self) -> &str;
    fn send(&self, request: &str) -> &'static str;
}

#[cfg(feature = "http")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpTransport {
    endpoint: String,
}

#[cfg(feature = "http")]
impl HttpTransport {
    #[must_use]
    pub fn connect(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
        }
    }
}

#[cfg(feature = "http")]
impl Transport for HttpTransport {
    const NAME: &'static str = "HTTP";

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn send(&self, _request: &str) -> &'static str {
        "HTTP request sent"
    }
}

#[cfg(feature = "ws")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsTransport {
    endpoint: String,
}

#[cfg(feature = "ws")]
impl WsTransport {
    #[must_use]
    pub fn connect(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
        }
    }
}

#[cfg(feature = "ws")]
impl Transport for WsTransport {
    const NAME: &'static str = "WebSocket";

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn send(&self, _request: &str) -> &'static str {
        "WebSocket request sent"
    }
}
