//! Slim extensions around established HTTP libraries.
//!
//! Server middleware and serving use axum. Response schemas and business
//! resources belong to applications.

pub mod server;

#[cfg(test)]
mod tests {
    use super::server::Config as ServerConfig;
    use super::server::*;
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
            Router::new().route(
                "/private",
                get(|Extension(mode): Extension<AuthenticationMode>| async move { mode.as_str() }),
            ),
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
        let body = to_bytes(accepted.into_body(), 1024)
            .await
            .expect("response body must be readable");
        assert_eq!(body.as_ref(), b"strict");
    }

    #[tokio::test]
    async fn configured_authentication_mode_replaces_existing_request_extension() {
        use AuthenticationMode::{GlobalTrusted, Strict};

        let routes = Router::new().route(
            "/private",
            get(|Extension(mode): Extension<AuthenticationMode>| async move { mode.as_str() }),
        );
        for (configured, previous) in [(Strict, GlobalTrusted), (GlobalTrusted, Strict)] {
            let token = BearerToken::new("correct-secret").expect("test token must be valid");
            let config = loopback_config(Some(token), RequestLimits::default())
                .with_authentication_mode(configured);
            let router = protected_router(routes.clone(), &config)
                .expect("test router configuration must be valid");

            let response = router
                .oneshot(
                    axum::http::Request::builder()
                        .uri("/private")
                        .extension(previous)
                        .header(axum::http::header::AUTHORIZATION, "Bearer correct-secret")
                        .body(Body::empty())
                        .expect("test request must build"),
                )
                .await
                .expect("router must respond");
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), 1024)
                .await
                .expect("response body must be readable");
            assert_eq!(body.as_ref(), configured.as_str().as_bytes());
        }
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

        for (path, status, authorization) in [
            (LIVENESS_PATH, StatusCode::NO_CONTENT, None),
            (
                LIVENESS_PATH,
                StatusCode::NO_CONTENT,
                Some("Bearer wrong-secret"),
            ),
            (READINESS_PATH, StatusCode::SERVICE_UNAVAILABLE, None),
            (
                READINESS_PATH,
                StatusCode::SERVICE_UNAVAILABLE,
                Some("Bearer wrong-secret"),
            ),
        ] {
            let mut request = axum::http::Request::builder().uri(path);
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
            assert_eq!(response.status(), status);
            let body = to_bytes(response.into_body(), 1024)
                .await
                .expect("health body must be readable");
            assert!(body.is_empty());
        }

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
    }
}
