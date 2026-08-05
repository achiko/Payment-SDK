use std::{fmt, sync::Arc};

use axum::{
    Json, Router,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use http_support::BearerToken;
use serde::Serialize;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrincipalRole {
    Exchange,
    Administrator,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthenticatedPrincipal {
    role: PrincipalRole,
}

impl AuthenticatedPrincipal {
    #[must_use]
    pub const fn role(self) -> PrincipalRole {
        self.role
    }

    /// Stable scope used for command idempotency. It deliberately identifies
    /// the configured credential role, not an end customer or bearer value.
    #[must_use]
    pub const fn idempotency_scope(self) -> &'static str {
        match self.role {
            PrincipalRole::Exchange => "exchange",
            PrincipalRole::Administrator => "administrator",
        }
    }
}

#[derive(Clone)]
pub struct Credentials {
    ordinary: BearerToken,
    administrator: BearerToken,
}

impl Credentials {
    #[must_use]
    pub const fn new(ordinary: BearerToken, administrator: BearerToken) -> Self {
        Self {
            ordinary,
            administrator,
        }
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Credentials([REDACTED])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequiredRole {
    Ordinary,
    Administrator,
}

#[derive(Clone)]
struct AuthState {
    credentials: Arc<Credentials>,
    required: RequiredRole,
}

/// Protects user-facing routes. The administrator credential is a strict
/// superset and may call these routes as required by the PS contract.
pub fn ordinary_routes<S>(router: Router<S>, credentials: Arc<Credentials>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router.layer(middleware::from_fn_with_state(
        AuthState {
            credentials,
            required: RequiredRole::Ordinary,
        },
        authenticate,
    ))
}

/// Protects administrator-only routes. A valid ordinary credential is
/// authenticated but forbidden, and therefore receives 403 rather than 401.
pub fn administrator_routes<S>(router: Router<S>, credentials: Arc<Credentials>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router.layer(middleware::from_fn_with_state(
        AuthState {
            credentials,
            required: RequiredRole::Administrator,
        },
        authenticate,
    ))
}

async fn authenticate(
    State(state): State<AuthState>,
    mut request: Request,
    next: Next,
) -> Response {
    let authorization = request.headers().get(header::AUTHORIZATION);
    let principal = if state
        .credentials
        .administrator
        .matches_authorization_header(authorization)
    {
        Some(AuthenticatedPrincipal {
            role: PrincipalRole::Administrator,
        })
    } else if state
        .credentials
        .ordinary
        .matches_authorization_header(authorization)
    {
        Some(AuthenticatedPrincipal {
            role: PrincipalRole::Exchange,
        })
    } else {
        None
    };

    match (state.required, principal) {
        (_, None) => authentication_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "valid bearer authentication is required",
        ),
        (RequiredRole::Administrator, Some(principal))
            if principal.role != PrincipalRole::Administrator =>
        {
            authentication_error(
                StatusCode::FORBIDDEN,
                "forbidden",
                "administrator authorization is required",
            )
        }
        (_, Some(principal)) => {
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
    }
}

#[derive(Serialize)]
struct ErrorEnvelope {
    code: &'static str,
    message: &'static str,
    retryable: bool,
    request_id: String,
}

fn authentication_error(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    let mut response = (
        status,
        Json(ErrorEnvelope {
            code,
            message,
            retryable: false,
            request_id: format!("ps-request-{}", Uuid::now_v7()),
        }),
    )
        .into_response();
    if status == StatusCode::UNAUTHORIZED {
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            "Bearer".parse().expect("static header value must parse"),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        extract::Extension,
        http::Request,
        routing::get,
    };
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;

    fn credentials() -> Arc<Credentials> {
        Arc::new(Credentials::new(
            BearerToken::new("ordinary-secret").expect("test token must be valid"),
            BearerToken::new("admin-secret").expect("test token must be valid"),
        ))
    }

    fn router() -> Router {
        let credentials = credentials();
        let ordinary = ordinary_routes(
            Router::new().route(
                "/ordinary",
                get(
                    |Extension(principal): Extension<AuthenticatedPrincipal>| async move {
                        principal.idempotency_scope()
                    },
                ),
            ),
            Arc::clone(&credentials),
        );
        let administrator = administrator_routes(
            Router::new().route(
                "/admin",
                get(
                    |Extension(principal): Extension<AuthenticatedPrincipal>| async move {
                        principal.idempotency_scope()
                    },
                ),
            ),
            credentials,
        );
        ordinary.merge(administrator)
    }

    async fn call(path: &str, token: Option<&str>) -> Response {
        let mut builder = Request::builder().uri(path);
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        router()
            .oneshot(
                builder
                    .body(Body::empty())
                    .expect("test request must build"),
            )
            .await
            .expect("router must respond")
    }

    #[tokio::test]
    async fn ordinary_and_administrator_credentials_have_expected_access() {
        let ordinary = call("/ordinary", Some("ordinary-secret")).await;
        assert_eq!(ordinary.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(ordinary.into_body(), 64)
                .await
                .expect("body must be readable"),
            "exchange"
        );

        let admin_on_ordinary = call("/ordinary", Some("admin-secret")).await;
        assert_eq!(admin_on_ordinary.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(admin_on_ordinary.into_body(), 64)
                .await
                .expect("body must be readable"),
            "administrator"
        );

        let admin = call("/admin", Some("admin-secret")).await;
        assert_eq!(admin.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ordinary_credential_is_forbidden_from_administrator_routes() {
        let response = call("/admin", Some("ordinary-secret")).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = to_bytes(response.into_body(), 4096)
            .await
            .expect("body must be readable");
        let envelope: Value = serde_json::from_slice(&body).expect("body must be JSON");
        assert_eq!(envelope["code"], "forbidden");
        assert_eq!(envelope["retryable"], false);
        assert!(
            envelope["request_id"]
                .as_str()
                .expect("request ID must be a string")
                .starts_with("ps-request-")
        );
    }

    #[tokio::test]
    async fn missing_or_wrong_credentials_are_unauthorized_and_redacted() {
        for token in [None, Some("wrong-secret")] {
            let response = call("/ordinary", token).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(
                response
                    .headers()
                    .get(header::WWW_AUTHENTICATE)
                    .expect("challenge header must exist"),
                "Bearer"
            );
            let body = to_bytes(response.into_body(), 4096)
                .await
                .expect("body must be readable");
            let text = std::str::from_utf8(&body).expect("body must be UTF-8");
            assert!(text.contains("\"code\":\"unauthorized\""));
            assert!(!text.contains("wrong-secret"));
            assert!(!text.contains("ordinary-secret"));
            assert!(!text.contains("admin-secret"));
        }
    }

    #[test]
    fn credential_debug_output_is_redacted() {
        let output = format!("{:?}", credentials());
        assert_eq!(output, "Credentials([REDACTED])");
        assert!(!output.contains("secret"));
    }
}
