use std::{error::Error, fmt, str::FromStr, sync::Arc};

use axum::http::HeaderValue;

use super::{ConfigError, ConfigErrorKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthenticationMode {
    Strict,
    GlobalTrusted,
}

impl AuthenticationMode {
    #[must_use]
    pub const fn is_strict(self) -> bool {
        matches!(self, Self::Strict)
    }

    /// Sanitized value exposed through readiness, status, and logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::GlobalTrusted => "global_trusted",
        }
    }
}

impl fmt::Display for AuthenticationMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for AuthenticationMode {
    type Err = AuthenticationModeParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "true" => Ok(Self::Strict),
            "false" => Ok(Self::GlobalTrusted),
            _ => Err(AuthenticationModeParseError::InvalidValue),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthenticationModeParseError {
    InvalidValue,
}

impl fmt::Display for AuthenticationModeParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "authentication mode must be exactly `true` (strict) or `false` (global trusted)",
        )
    }
}

impl Error for AuthenticationModeParseError {}

#[derive(Clone, PartialEq, Eq)]
pub struct BearerToken(Arc<[u8]>);

impl BearerToken {
    pub fn new(token: impl AsRef<str>) -> Result<Self, ConfigError> {
        let bytes = token.as_ref().as_bytes();
        if bytes.is_empty()
            || bytes
                .iter()
                .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        {
            return Err(ConfigError::new(
                ConfigErrorKind::InvalidBearerToken,
                "bearer token must be non-empty and contain no whitespace or control characters",
            ));
        }
        Ok(Self(Arc::from(bytes)))
    }

    /// Compares equal-length credentials using `constant_time_eq`.
    /// Missing headers, invalid prefixes, and unequal lengths may return early;
    /// credential length is not hidden.
    #[must_use]
    pub fn matches_authorization_header(&self, value: Option<&HeaderValue>) -> bool {
        const PREFIX: &[u8] = b"Bearer ";
        let Some(value) = value else {
            return false;
        };
        let bytes = value.as_bytes();
        if bytes.len() < PREFIX.len() || !bytes.starts_with(PREFIX) {
            return false;
        }
        constant_time_eq::constant_time_eq(&bytes[PREFIX.len()..], &self.0)
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BearerToken([REDACTED])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_requires_every_credential_byte_to_match() {
        let token = BearerToken::new("correct-secret").expect("fixture token is valid");
        assert!(
            token.matches_authorization_header(Some(&HeaderValue::from_static(
                "Bearer correct-secret",
            )))
        );

        for value in [
            "Bearer xorrect-secret",
            "Bearer correctxsecret",
            "Bearer correct-secrex",
        ] {
            assert!(!token.matches_authorization_header(Some(&HeaderValue::from_static(value))));
        }
    }

    #[test]
    fn authorization_rejects_shorter_longer_and_empty_credentials() {
        let token = BearerToken::new("correct-secret").expect("fixture token is valid");
        for value in [
            "Bearer correct-secre",
            "Bearer correct-secret-extra",
            "Bearer ",
        ] {
            assert!(!token.matches_authorization_header(Some(&HeaderValue::from_static(value))));
        }
    }

    #[test]
    fn authorization_requires_the_exact_bearer_prefix_without_trimming() {
        let token = BearerToken::new("correct-secret").expect("fixture token is valid");
        assert!(!token.matches_authorization_header(None));
        for value in [
            "",
            "Bearer",
            "correct-secret",
            "Basic correct-secret",
            "bearer correct-secret",
            "Bearer  correct-secret",
            "Bearer correct-secret ",
        ] {
            assert!(!token.matches_authorization_header(Some(&HeaderValue::from_static(value))));
        }
    }
}
