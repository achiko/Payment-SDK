use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use base::{Address, Addresser, Broadcaster, SignRequest, TransactionBuilder};
use http_body_util::BodyExt;
use tower::ServiceExt;
use wallets::{
    AddressEncoding, AddressFormat, AddressText, BalanceReader, FutureResult, HistoryReader,
    Provider, SecretBytes, TransactionFactory,
};

use super::*;
use crate::{TransactionPage, TransferResponse};

enum FixtureProvider {
    Value,
}

impl Provider for FixtureProvider {
    fn create<'a>(&'a self, _secret: SecretBytes) -> FutureResult<'a, Arc<dyn wallets::Wallet>> {
        Box::pin(async { Ok(Arc::new(FixtureWallet::Value) as Arc<dyn wallets::Wallet>) })
    }
}

enum FixtureWallet {
    Value,
}

enum FixtureIndex {
    Value,
    Reject,
}

enum FixtureTransactions {
    Value,
}

impl wallets::Sender for FixtureTransactions {
    fn send<'a>(&'a self, _transfers: Vec<wallets::Transfer>) -> wallets::SendFuture<'a> {
        Box::pin(async { Ok(vec![base::TransactionId::new("fixture-transaction")]) })
    }
}

impl indexing::Checkpoint for FixtureIndex {
    fn checkpoint<'a>(
        &'a self,
        _scope: &'a indexing::IndexScope,
    ) -> indexing::BoxFuture<'a, Result<Option<indexing::BlockRef>, indexing::IndexError>> {
        Box::pin(async { Ok(None) })
    }
}

impl indexing::Watcher for FixtureIndex {
    fn watch<'a>(
        &'a self,
        request: indexing::WatchRequest,
    ) -> indexing::BoxFuture<'a, Result<indexing::WatchReceipt, indexing::IndexError>> {
        if matches!(self, Self::Reject) {
            return Box::pin(async {
                Err(indexing::IndexError::new(
                    indexing::IndexErrorKind::Store,
                    "fixture watch rejection",
                    true,
                ))
            });
        }
        Box::pin(async move {
            Ok(indexing::WatchReceipt {
                id: indexing::WatchId("fixture-watch".to_owned()),
                scope: request.scope,
                selector: request.selector,
                start_height: request.start_height,
                registered_at: None,
            })
        })
    }
}

impl Addresser for FixtureWallet {
    fn address(&self) -> Address {
        Address::from([7_u8; 20])
    }
}

impl base::Signer for FixtureWallet {
    fn sign<'a>(&'a self, _request: SignRequest) -> base::SignFuture<'a> {
        Box::pin(async { unreachable!("route fixture never signs") })
    }
}

impl AddressFormat for FixtureWallet {
    fn address_text(&self, _address: &Address) -> Result<AddressText, wallets::Error> {
        Ok(AddressText::new(AddressEncoding::Hex, "fixture-address"))
    }

    fn parse_address(&self, _address: &AddressText) -> Result<Address, wallets::Error> {
        Ok(Address::from([8_u8; 20]))
    }
}

impl BalanceReader for FixtureWallet {
    fn balance<'a>(&'a self) -> FutureResult<'a, wallets::Balance> {
        Box::pin(async {
            Ok(wallets::Balance {
                amount: "12.5".parse().expect("fixture decimal"),
                observed_at: None,
            })
        })
    }
}

impl HistoryReader for FixtureWallet {
    fn history<'a>(&'a self, _request: HistoryRequest) -> FutureResult<'a, wallets::History> {
        Box::pin(async {
            Ok(wallets::History {
                transactions: Vec::new(),
                next: None,
            })
        })
    }
}

impl TransactionFactory for FixtureWallet {
    fn transaction(&self) -> Box<dyn TransactionBuilder> {
        unreachable!("route fixture never builds a transaction")
    }

    fn restore(
        &self,
        _snapshot: &base::TransactionSnapshot,
    ) -> Result<Box<dyn TransactionBuilder>, base::TransactionError> {
        unreachable!("fixture operation must not run")
    }

    fn broadcaster(&self) -> &dyn Broadcaster {
        unreachable!("route fixture never broadcasts")
    }
}

fn app() -> Router {
    app_with_index(FixtureIndex::Value)
}

fn app_with_index(index: FixtureIndex) -> Router {
    let token =
        http_support::server::BearerToken::new("secret").expect("fixture bearer must be valid");
    let config = http_support::server::Config::new(
        "127.0.0.1:0".parse().expect("fixture bind"),
        http_support::server::TransportSecurity::PlaintextLoopback,
        Some(token),
        http_support::server::RequestLimits::default(),
    );
    let mut providers = wallets::Providers::new();
    providers
        .register(Chain::Bitcoin, FixtureProvider::Value)
        .expect("fixture provider must register");
    let mut api = Gateway::new(providers);
    let index = Arc::new(index);
    api.register(WalletFamily {
        chain: Chain::Bitcoin,
        network: "regtest".to_owned(),
        scope: indexing::IndexScope {
            chain: indexing::ChainId("bitcoin".to_owned()),
            network: "regtest".to_owned(),
        },
        watcher: index.clone(),
        checkpoint: index,
        transactions: Arc::new(FixtureTransactions::Value),
    })
    .expect("fixture family must register");
    api.router(&config).expect("fixture router must build")
}

async fn request(app: Router, method: &str, path: &str, body: &str) -> ResponseParts {
    let response = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(body.to_owned()))
                .expect("fixture request must build"),
        )
        .await
        .expect("route must respond");
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body must collect")
        .to_bytes();
    ResponseParts { status, body }
}

struct ResponseParts {
    status: StatusCode,
    body: axum::body::Bytes,
}

#[tokio::test]
async fn generated_wallet_can_be_read_with_balance_and_history() {
    let app = app();
    let created = request(app.clone(), "POST", "/v1/wallets", r#"{"chain":"bitcoin"}"#).await;
    assert_eq!(created.status, StatusCode::CREATED);
    let wallet: Wallet = serde_json::from_slice(&created.body).expect("wallet response");
    assert_eq!(wallet.network, "regtest");
    assert_eq!(wallet.address, "fixture-address");

    let read = request(
        app.clone(),
        "GET",
        &format!("/v1/wallets/{}", wallet.id),
        "",
    )
    .await;
    assert_eq!(read.status, StatusCode::OK);

    let balance = request(
        app.clone(),
        "GET",
        &format!("/v1/wallets/{}/balance", wallet.id),
        "",
    )
    .await;
    assert_eq!(balance.status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Balance>(&balance.body)
            .expect("balance response")
            .amount,
        "12.5"
    );

    let history = request(
        app,
        "GET",
        &format!("/v1/wallets/{}/transactions", wallet.id),
        "",
    )
    .await;
    assert_eq!(history.status, StatusCode::OK);
    assert!(
        serde_json::from_slice::<TransactionPage>(&history.body)
            .expect("history response")
            .transactions
            .is_empty()
    );
}

#[tokio::test]
async fn protected_routes_require_authentication() {
    let response = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/wallets")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"chain":"bitcoin"}"#))
                .expect("fixture request"),
        )
        .await
        .expect("route response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wallet_is_not_returned_before_its_watch_is_durable() {
    let response = request(
        app_with_index(FixtureIndex::Reject),
        "POST",
        "/v1/wallets",
        r#"{"chain":"bitcoin"}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn send_rejects_an_inexact_amount_before_building() {
    let app = app();
    let created = request(app.clone(), "POST", "/v1/wallets", r#"{"chain":"bitcoin"}"#).await;
    let wallet: Wallet = serde_json::from_slice(&created.body).expect("wallet response");
    let response = request(
        app,
        "POST",
        &format!("/v1/wallets/{}/transactions", wallet.id),
        r#"{"destination":{"encoding":"hex","text":"destination"},"amount":"not-a-number"}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn transaction_batch_uses_the_registered_chain_service() {
    let app = app();
    let created = request(app.clone(), "POST", "/v1/wallets", r#"{"chain":"bitcoin"}"#).await;
    let wallet: Wallet = serde_json::from_slice(&created.body).expect("wallet response");
    let body = serde_json::json!({
        "transfers": [
            {
                "wallet_id": wallet.id,
                "destination": {"encoding": "hex", "text": "first"},
                "amount": "1"
            },
            {
                "wallet_id": wallet.id,
                "destination": {"encoding": "hex", "text": "second"},
                "amount": "2"
            }
        ]
    });
    let response = request(app, "POST", "/v1/transactions", &body.to_string()).await;
    assert_eq!(response.status, StatusCode::ACCEPTED);
    assert_eq!(
        serde_json::from_slice::<TransferResponse>(&response.body)
            .expect("batch response")
            .transaction_ids,
        ["fixture-transaction"]
    );
}

#[test]
fn transaction_wallet_errors_remain_transaction_errors() {
    let error = wallet_error(wallets::Error::new(
        wallets::ErrorKind::Transaction,
        "node rejected transaction",
    ));
    assert_eq!(error.kind, crate::ErrorKind::Transaction);
}
