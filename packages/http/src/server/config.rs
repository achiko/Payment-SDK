use std::{error::Error, fmt, net::SocketAddr};

use super::{AuthenticationMode, BearerToken};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportSecurity {
    /// Plain HTTP is permitted only when the listener is bound to loopback.
    PlaintextLoopback,
    /// A trusted upstream proxy is responsible for TLS termination.
    TlsTerminatedUpstream,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestLimits {
    max_body_bytes: usize,
}

impl RequestLimits {
    pub fn new(max_body_bytes: usize) -> Result<Self, ConfigError> {
        if max_body_bytes == 0 {
            return Err(ConfigError::new(
                ConfigErrorKind::InvalidLimits,
                "maximum request body size must be greater than zero",
            ));
        }
        Ok(Self { max_body_bytes })
    }

    #[must_use]
    pub const fn max_body_bytes(&self) -> usize {
        self.max_body_bytes
    }
}

impl Default for RequestLimits {
    fn default() -> Self {
        Self {
            max_body_bytes: 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub(super) bind_addr: SocketAddr,
    transport_security: TransportSecurity,
    pub(super) authentication_mode: AuthenticationMode,
    pub(super) bearer_token: Option<BearerToken>,
    custom_authentication: bool,
    pub(super) limits: RequestLimits,
}

impl Config {
    #[must_use]
    pub fn new(
        bind_addr: SocketAddr,
        transport_security: TransportSecurity,
        bearer_token: Option<BearerToken>,
        limits: RequestLimits,
    ) -> Self {
        Self {
            bind_addr,
            transport_security,
            authentication_mode: AuthenticationMode::Strict,
            bearer_token,
            custom_authentication: false,
            limits,
        }
    }

    #[must_use]
    pub const fn with_authentication_mode(
        mut self,
        authentication_mode: AuthenticationMode,
    ) -> Self {
        self.authentication_mode = authentication_mode;
        self
    }

    /// Declares that the application router installs its own authentication
    /// middleware, for example when it supports multiple credential roles.
    #[must_use]
    pub const fn with_custom_authentication(mut self) -> Self {
        self.custom_authentication = true;
        self
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.bind_addr.ip().is_loopback()
            && self.transport_security != TransportSecurity::TlsTerminatedUpstream
        {
            return Err(ConfigError::new(
                ConfigErrorKind::InsecureNonLoopbackBind,
                "a non-loopback listener requires upstream TLS termination",
            ));
        }
        if self.authentication_mode.is_strict()
            && self.bearer_token.is_none()
            && !self.custom_authentication
        {
            return Err(ConfigError::new(
                ConfigErrorKind::MissingBearerToken,
                "strict authentication requires a bearer or a declared application authorizer",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub const fn bind_addr(&self) -> SocketAddr {
        self.bind_addr
    }

    #[must_use]
    pub const fn limits(&self) -> &RequestLimits {
        &self.limits
    }

    #[must_use]
    pub const fn authentication_mode(&self) -> AuthenticationMode {
        self.authentication_mode
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigErrorKind {
    InvalidLimits,
    InvalidBearerToken,
    InsecureNonLoopbackBind,
    MissingBearerToken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    pub kind: ConfigErrorKind,
    pub message: String,
}

impl ConfigError {
    pub(super) fn new(kind: ConfigErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ConfigError {}
