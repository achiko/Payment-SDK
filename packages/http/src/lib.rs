//! Slim extensions around established HTTP libraries.
//!
//! Client execution is backed by reqwest. Server middleware and serving use axum.
//! This crate contains transport mechanics only; response schemas and business
//! resources belong to applications.

pub mod client;
pub mod server;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::server::Config as ServerConfig;
    use super::{client::Config as ClientConfig, client::ErrorKind as ClientErrorKind};
    use super::{client::*, server::*};
    use crate::client::ResponseBody;
    use axum::{
        Router,
        body::{Body, to_bytes},
        extract::Extension,
        http::StatusCode,
        routing::{get, post},
    };
    use tower::ServiceExt;

    fn loopback_config(token: Option<BearerToken>, limits: RequestLimits) -> ServerConfig {
        ServerConfig::new(
            "127.0.0.1:0"
                .parse()
                .expect("test loopback socket address must parse"),
            TransportSecurity::PlaintextLoopback,
            token,
            limits,
        )
    }

    #[test]
    fn authentication_mode_accepts_only_exact_lowercase_boolean_values() {
        assert_eq!("true".parse(), Ok(AuthenticationMode::Strict));
        assert_eq!("false".parse(), Ok(AuthenticationMode::GlobalTrusted));

        for invalid in ["", " true", "true ", "TRUE", "False", "1", "yes"] {
            assert!(invalid.parse::<AuthenticationMode>().is_err());
        }
    }

    #[test]
    fn non_loopback_bind_accepts_declared_application_authentication() {
        let config = ServerConfig::new(
            "0.0.0.0:8443"
                .parse()
                .expect("test socket address must parse"),
            TransportSecurity::TlsTerminatedUpstream,
            None,
            RequestLimits::default(),
        )
        .with_custom_authentication();

        config
            .validate()
            .expect("TLS plus declared application authentication must be accepted");
    }

    #[tokio::test]
    async fn protected_routes_require_the_exact_bearer_token() {
        let token = BearerToken::new("correct-secret").expect("test token must be valid");
        let router = service_router(
            Router::new().route("/private", get(|| async { "ok" })),
            &loopback_config(Some(token), RequestLimits::default()),
            HealthState::new(true),
        )
        .expect("test router configuration must be valid");

        let missing = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/private")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
        let missing_body = to_bytes(missing.into_body(), 1024)
            .await
            .expect("authentication body must be readable");
        assert!(missing_body.is_empty());

        let wrong = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/private")
                    .header(axum::http::header::AUTHORIZATION, "Bearer wrong-secret")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
        let wrong_body = to_bytes(wrong.into_body(), 1024)
            .await
            .expect("authentication body must be readable");
        assert!(wrong_body.is_empty());

        let accepted = router
            .oneshot(
                axum::http::Request::builder()
                    .uri("/private")
                    .header(axum::http::header::AUTHORIZATION, "Bearer correct-secret")
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(accepted.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn global_trusted_routes_ignore_authorization_headers() {
        let token = BearerToken::new("configured-but-ignored").expect("test token must be valid");
        let config = loopback_config(Some(token), RequestLimits::default())
            .with_authentication_mode(AuthenticationMode::GlobalTrusted);
        let router = service_router(
            Router::new().route(
                "/private",
                get(|Extension(mode): Extension<AuthenticationMode>| async move { mode.as_str() }),
            ),
            &config,
            HealthState::new(true),
        )
        .expect("global-trusted router configuration must be valid");

        for authorization in [None, Some("Bearer wrong-secret")] {
            let mut request = axum::http::Request::builder().uri("/private");
            if let Some(value) = authorization {
                request = request.header(axum::http::header::AUTHORIZATION, value);
            }
            let response = router
                .clone()
                .oneshot(
                    request
                        .body(Body::empty())
                        .expect("test request must build"),
                )
                .await
                .expect("router must respond");
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), 1024)
                .await
                .expect("response body must be readable");
            assert_eq!(body.as_ref(), b"global_trusted");
        }

        let readiness = router
            .oneshot(
                axum::http::Request::builder()
                    .uri(READINESS_PATH)
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(readiness.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn health_routes_are_unauthenticated_and_sanitized() {
        let token = BearerToken::new("secret").expect("test token must be valid");
        let health = HealthState::new(false);
        let router = service_router(
            Router::new().route("/private", get(|| async { "private" })),
            &loopback_config(Some(token), RequestLimits::default()),
            health.clone(),
        )
        .expect("test router configuration must be valid");

        let live = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(LIVENESS_PATH)
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(live.status(), StatusCode::NO_CONTENT);
        let live_body = to_bytes(live.into_body(), 1024)
            .await
            .expect("health body must be readable");
        assert!(live_body.is_empty());

        let not_ready = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(READINESS_PATH)
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(not_ready.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(not_ready.into_body(), 1024)
            .await
            .expect("health body must be readable");
        assert!(body.is_empty());

        health.set_ready(true);
        let ready = router
            .oneshot(
                axum::http::Request::builder()
                    .uri(READINESS_PATH)
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(ready.status(), StatusCode::NO_CONTENT);
        let body = to_bytes(ready.into_body(), 1024)
            .await
            .expect("health body must be readable");
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn configured_body_limit_rejects_large_requests() {
        let limits = RequestLimits::new(4).expect("test limits must be valid");
        let router = service_router(
            Router::new().route("/echo", post(|body: String| async move { body })),
            &loopback_config(None, limits)
                .with_authentication_mode(AuthenticationMode::GlobalTrusted),
            HealthState::new(true),
        )
        .expect("test router configuration must be valid");

        let response = router
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/echo")
                    .header(axum::http::header::CONTENT_TYPE, "text/plain")
                    .body(Body::from("12345"))
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond");
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn listeners_require_mode_appropriate_security() {
        let address = "0.0.0.0:8080"
            .parse()
            .expect("test non-loopback socket address must parse");
        let insecure = ServerConfig::new(
            address,
            TransportSecurity::PlaintextLoopback,
            None,
            RequestLimits::default(),
        );
        assert_eq!(
            insecure
                .validate()
                .expect_err("insecure bind must fail")
                .kind,
            ConfigErrorKind::InsecureNonLoopbackBind
        );

        let unauthenticated = ServerConfig::new(
            address,
            TransportSecurity::TlsTerminatedUpstream,
            None,
            RequestLimits::default(),
        );
        assert_eq!(
            unauthenticated
                .validate()
                .expect_err("unauthenticated bind must fail")
                .kind,
            ConfigErrorKind::MissingBearerToken
        );

        let loopback_without_bearer = ServerConfig::new(
            "127.0.0.1:8080"
                .parse()
                .expect("test loopback socket address must parse"),
            TransportSecurity::PlaintextLoopback,
            None,
            RequestLimits::default(),
        );
        assert_eq!(
            loopback_without_bearer
                .validate()
                .expect_err("strict loopback routes must not fail open")
                .kind,
            ConfigErrorKind::MissingBearerToken
        );

        ServerConfig::new(
            address,
            TransportSecurity::TlsTerminatedUpstream,
            None,
            RequestLimits::default(),
        )
        .with_authentication_mode(AuthenticationMode::GlobalTrusted)
        .validate()
        .expect("global-trusted non-loopback listener still requires TLS but not a bearer");
    }

    #[test]
    fn body_limits_are_configurable() {
        let limits = RequestLimits::new(128).expect("test limits must be valid");
        assert_eq!(limits.max_body_bytes(), 128);
    }

    #[test]
    fn debug_output_never_contains_credentials() {
        let token = BearerToken::new("top-secret").expect("test token must be valid");
        assert!(!format!("{token:?}").contains("top-secret"));

        let config = ClientConfig {
            endpoint: "https://user:password@example.invalid".to_owned(),
            request_timeout: Duration::from_secs(1),
            max_response_bytes: 1024,
            default_headers: vec![("authorization".to_owned(), "Bearer hidden".to_owned())],
            retry_policy: Retry::default(),
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("password"));
        assert!(!debug.contains("Bearer hidden"));
        assert!(debug.contains("authorization"));

        let transport = Reqwest::new(config).expect("test transport must build");
        let debug = format!("{transport:?}");
        assert!(!debug.contains("password"));
        assert!(!debug.contains("Bearer hidden"));
        assert!(debug.contains("authorization"));
    }

    #[tokio::test]
    async fn rejected_request_metadata_does_not_leak_credentials_or_body() {
        let transport = Reqwest::new(ClientConfig::new(
            "https://user:password@example.invalid",
            Duration::from_secs(1),
        ))
        .expect("test transport must build");
        let error = transport
            .execute(Request {
                method: "POST".to_owned(),
                endpoint: String::new(),
                headers: vec![(
                    "authorization".to_owned(),
                    "Bearer hidden\ninvalid".to_owned(),
                )],
                body: b"sensitive-request-body".to_vec(),
            })
            .await
            .expect_err("an invalid header value must be rejected before sending");

        assert_eq!(error.kind, ClientErrorKind::Rejected);
        for secret in ["password", "Bearer hidden", "sensitive-request-body"] {
            assert!(!error.message.contains(secret));
        }
    }

    #[tokio::test]
    async fn http_transport_does_not_follow_redirects() {
        use std::io::{Read, Write};

        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("test redirect listener must bind");
        let address = listener
            .local_addr()
            .expect("test redirect listener address must be available");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("test request must connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .expect("test request read timeout must configure");
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/must-not-follow\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .expect("test redirect response must write");
        });
        let transport = Reqwest::new(ClientConfig::new(
            format!("http://{address}/rpc"),
            Duration::from_secs(1),
        ))
        .expect("test HTTP transport must build");

        let response = transport
            .execute(Request {
                method: "POST".to_owned(),
                endpoint: String::new(),
                headers: Vec::new(),
                body: b"{}".to_vec(),
            })
            .await
            .expect("the original redirect response must be returned");

        assert_eq!(response.status, 302);
        server.join().expect("test redirect server must stop");
    }

    #[test]
    fn bounded_response_rejects_declared_and_streamed_overflow_without_leaking_body() {
        let declared_error = match ResponseBody::new(4, Some(5)) {
            Ok(_) => panic!("an oversized declared response must fail"),
            Err(error) => error,
        };
        assert_eq!(declared_error.kind, ClientErrorKind::InvalidResponse);
        assert_eq!(
            declared_error.message,
            "HTTP response exceeds the configured size limit"
        );

        let mut streamed =
            ResponseBody::new(6, None).expect("an unknown response length is allowed");
        streamed
            .push_chunk(b"secret")
            .expect("a chunk at the limit must be accepted");
        let streamed_error = streamed
            .push_chunk(b"-response-body")
            .expect_err("a chunk crossing the response limit must fail");

        assert_eq!(streamed_error.kind, ClientErrorKind::InvalidResponse);
        assert!(!streamed_error.message.contains("secret"));
        assert!(!streamed_error.message.contains("response-body"));
    }

    #[test]
    fn bounded_response_accepts_multiple_chunks_at_the_exact_limit() {
        let mut response =
            ResponseBody::new(5, Some(5)).expect("the declared length is within limit");
        response
            .push_chunk(b"12")
            .expect("the first response chunk must fit");
        response
            .push_chunk(b"345")
            .expect("the final response chunk must reach the exact limit");

        assert_eq!(response.into_bytes(), b"12345");
    }
}
