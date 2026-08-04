use std::{fmt, sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use indexing::{BoxFuture, SourceError};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

use super::{EthereumHeadWake, source::parse_new_heads_notification};

const SUBSCRIPTION_REQUEST_ID: u64 = 1;

/// Runtime configuration for the optional Ethereum `newHeads` wake stream.
///
/// A missing URL disables the stream. This stream is only a latency hint: the
/// authoritative worker must reconcile every wake through numbered HTTP reads.
#[derive(Clone, PartialEq, Eq)]
pub struct EthereumNewHeadsConfig {
    websocket_url: Option<String>,
    reconnect_delay: Duration,
}

impl fmt::Debug for EthereumNewHeadsConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EthereumNewHeadsConfig")
            .field("websocket_configured", &self.websocket_url.is_some())
            .field("reconnect_delay", &self.reconnect_delay)
            .finish()
    }
}

impl EthereumNewHeadsConfig {
    pub fn new(
        websocket_url: Option<String>,
        reconnect_delay: Duration,
    ) -> Result<Self, SourceError> {
        if let Some(url) = websocket_url.as_deref()
            && !(url.starts_with("ws://") || url.starts_with("wss://"))
        {
            return Err(source_error(
                "Ethereum newHeads URL must use ws:// or wss://",
                false,
            ));
        }
        if websocket_url.as_deref().is_some_and(str::is_empty) {
            return Err(source_error(
                "Ethereum newHeads URL must not be empty",
                false,
            ));
        }
        Ok(Self {
            websocket_url,
            reconnect_delay,
        })
    }

    #[must_use]
    pub fn disabled() -> Self {
        Self {
            websocket_url: None,
            reconnect_delay: Duration::from_secs(1),
        }
    }

    #[must_use]
    pub fn websocket_url(&self) -> Option<&str> {
        self.websocket_url.as_deref()
    }

    #[must_use]
    pub fn reconnect_delay(&self) -> Duration {
        self.reconnect_delay
    }
}

/// Minimal message boundary used to make reconnect behavior deterministic in
/// tests without exposing Tungstenite types to the application.
pub trait EthereumNewHeadsConnection: Send {
    fn send<'a>(&'a mut self, payload: Vec<u8>) -> BoxFuture<'a, Result<(), SourceError>>;

    fn receive<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<Vec<u8>>, SourceError>>;
}

/// Connector boundary for the optional `newHeads` wake stream.
pub trait EthereumNewHeadsConnector: Send + Sync {
    fn connect<'a>(
        &'a self,
        websocket_url: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn EthereumNewHeadsConnection>, SourceError>>;
}

/// Tokio/Tungstenite connector used by the production wake-only client.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioTungsteniteNewHeadsConnector;

impl EthereumNewHeadsConnector for TokioTungsteniteNewHeadsConnector {
    fn connect<'a>(
        &'a self,
        websocket_url: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn EthereumNewHeadsConnection>, SourceError>> {
        Box::pin(async move {
            let (stream, _) = connect_async(websocket_url)
                .await
                .map_err(|_| source_error("Ethereum newHeads WebSocket connection failed", true))?;
            Ok(Box::new(TungsteniteNewHeadsConnection { stream })
                as Box<dyn EthereumNewHeadsConnection>)
        })
    }
}

struct TungsteniteNewHeadsConnection {
    stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl EthereumNewHeadsConnection for TungsteniteNewHeadsConnection {
    fn send<'a>(&'a mut self, payload: Vec<u8>) -> BoxFuture<'a, Result<(), SourceError>> {
        Box::pin(async move {
            let payload = String::from_utf8(payload)
                .map_err(|_| source_error("Ethereum newHeads request is not valid UTF-8", false))?;
            self.stream
                .send(Message::Text(payload.into()))
                .await
                .map_err(|_| source_error("Ethereum newHeads request send failed", true))
        })
    }

    fn receive<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<Vec<u8>>, SourceError>> {
        Box::pin(async move {
            loop {
                match self.stream.next().await {
                    Some(Ok(Message::Text(message))) => {
                        return Ok(Some(message.as_bytes().to_vec()));
                    }
                    Some(Ok(Message::Binary(message))) => return Ok(Some(message.to_vec())),
                    Some(Ok(Message::Ping(payload))) => {
                        self.stream
                            .send(Message::Pong(payload))
                            .await
                            .map_err(|_| {
                                source_error("Ethereum newHeads pong send failed", true)
                            })?;
                    }
                    Some(Ok(Message::Close(_))) | None => return Ok(None),
                    Some(Ok(Message::Pong(_) | Message::Frame(_))) => {}
                    Some(Err(_)) => {
                        return Err(source_error(
                            "Ethereum newHeads WebSocket receive failed",
                            true,
                        ));
                    }
                }
            }
        })
    }
}

/// Reconnecting wake-only Ethereum head subscriber.
///
/// `run` returns immediately when the URL is absent. Otherwise it reconnects
/// after transport closure, RPC rejection, or malformed session data. Returning
/// `false` from `on_wake` shuts the loop down cleanly. Head notifications never
/// become canonical block data; callers must wake the ordered HTTP worker.
pub struct EthereumNewHeadsClient<C = TokioTungsteniteNewHeadsConnector> {
    config: EthereumNewHeadsConfig,
    connector: Arc<C>,
}

impl EthereumNewHeadsClient<TokioTungsteniteNewHeadsConnector> {
    #[must_use]
    pub fn new(config: EthereumNewHeadsConfig) -> Self {
        Self::with_connector(config, TokioTungsteniteNewHeadsConnector)
    }
}

/// Sanitized lifecycle event for the optional wake-only WebSocket stream.
///
/// Events intentionally contain no endpoint, subscription identifier, or
/// provider error text, so applications can use them for metrics without
/// exposing RPC configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EthereumNewHeadsConnectionEvent {
    Connected,
    Disconnected,
    ReconnectScheduled,
    Failure,
}

impl<C> EthereumNewHeadsClient<C> {
    #[must_use]
    pub fn with_connector(config: EthereumNewHeadsConfig, connector: C) -> Self {
        Self {
            config,
            connector: Arc::new(connector),
        }
    }

    #[must_use]
    pub fn config(&self) -> &EthereumNewHeadsConfig {
        &self.config
    }
}

impl<C> EthereumNewHeadsClient<C>
where
    C: EthereumNewHeadsConnector,
{
    pub async fn run<F>(&self, mut on_wake: F) -> Result<(), SourceError>
    where
        F: FnMut(EthereumHeadWake) -> bool + Send,
    {
        self.run_with_events(&mut on_wake, |_| {}).await
    }

    /// Runs the wake stream while reporting sanitized connection lifecycle
    /// events. `Connected` means the subscription acknowledgement was accepted,
    /// not merely that the transport handshake completed.
    pub async fn run_with_events<F, E>(
        &self,
        mut on_wake: F,
        mut on_event: E,
    ) -> Result<(), SourceError>
    where
        F: FnMut(EthereumHeadWake) -> bool + Send,
        E: FnMut(EthereumNewHeadsConnectionEvent) + Send,
    {
        let Some(websocket_url) = self.config.websocket_url() else {
            return Ok(());
        };
        let request = subscription_request()?;

        loop {
            let mut connection = match self.connector.connect(websocket_url).await {
                Ok(connection) => connection,
                Err(_) => {
                    on_event(EthereumNewHeadsConnectionEvent::Failure);
                    on_event(EthereumNewHeadsConnectionEvent::ReconnectScheduled);
                    tokio::time::sleep(self.config.reconnect_delay()).await;
                    continue;
                }
            };
            if connection.send(request.clone()).await.is_err() {
                on_event(EthereumNewHeadsConnectionEvent::Failure);
                on_event(EthereumNewHeadsConnectionEvent::ReconnectScheduled);
                tokio::time::sleep(self.config.reconnect_delay()).await;
                continue;
            }

            let mut session = NewHeadsSession::awaiting_confirmation();
            let mut connected = false;
            loop {
                let message = match connection.receive().await {
                    Ok(Some(message)) => message,
                    Ok(None) => break,
                    Err(_) => {
                        on_event(EthereumNewHeadsConnectionEvent::Failure);
                        break;
                    }
                };
                let was_active = session.is_active();
                match session.consume(&message) {
                    Ok(Some(wake)) if !on_wake(wake) => {
                        if connected {
                            on_event(EthereumNewHeadsConnectionEvent::Disconnected);
                        }
                        return Ok(());
                    }
                    Ok(_) => {
                        if !was_active && session.is_active() {
                            connected = true;
                            on_event(EthereumNewHeadsConnectionEvent::Connected);
                        }
                    }
                    Err(_) => {
                        on_event(EthereumNewHeadsConnectionEvent::Failure);
                        break;
                    }
                }
            }

            if connected {
                on_event(EthereumNewHeadsConnectionEvent::Disconnected);
            }
            on_event(EthereumNewHeadsConnectionEvent::ReconnectScheduled);

            tokio::time::sleep(self.config.reconnect_delay()).await;
        }
    }
}

enum NewHeadsSession {
    AwaitingConfirmation,
    Active { subscription_id: String },
}

impl NewHeadsSession {
    fn awaiting_confirmation() -> Self {
        Self::AwaitingConfirmation
    }

    fn is_active(&self) -> bool {
        matches!(self, Self::Active { .. })
    }

    fn consume(&mut self, message: &[u8]) -> Result<Option<EthereumHeadWake>, SourceError> {
        match self {
            Self::AwaitingConfirmation => {
                let value: Value = serde_json::from_slice(message).map_err(|_| {
                    source_error("Ethereum newHeads response is not valid JSON", true)
                })?;
                if value.get("id").and_then(Value::as_u64) != Some(SUBSCRIPTION_REQUEST_ID) {
                    return Err(source_error(
                        "Ethereum newHeads response ID does not match its request",
                        true,
                    ));
                }
                if value.get("error").is_some() {
                    return Err(source_error(
                        "Ethereum node rejected the newHeads subscription",
                        true,
                    ));
                }
                let subscription_id = value
                    .get("result")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| {
                        source_error("Ethereum newHeads response has no subscription ID", true)
                    })?
                    .to_owned();
                *self = Self::Active { subscription_id };
                Ok(None)
            }
            Self::Active { subscription_id } => {
                let (message_subscription_id, wake) = parse_new_heads_notification(message)?;
                if message_subscription_id == subscription_id.as_str() {
                    Ok(Some(wake))
                } else {
                    Ok(None)
                }
            }
        }
    }
}

fn subscription_request() -> Result<Vec<u8>, SourceError> {
    serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": SUBSCRIPTION_REQUEST_ID,
        "method": "eth_subscribe",
        "params": ["newHeads"],
    }))
    .map_err(|_| source_error("Ethereum newHeads request could not be encoded", false))
}

fn source_error(message: &'static str, retryable: bool) -> SourceError {
    SourceError {
        message: message.to_owned(),
        retryable,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use indexing::BlockHeight;

    use super::*;

    #[derive(Clone)]
    struct ScriptedConnector {
        connections: Arc<Mutex<VecDeque<Box<dyn EthereumNewHeadsConnection>>>>,
        connect_count: Arc<Mutex<usize>>,
        failures_before_connect: Arc<Mutex<usize>>,
    }

    impl EthereumNewHeadsConnector for ScriptedConnector {
        fn connect<'a>(
            &'a self,
            _websocket_url: &'a str,
        ) -> BoxFuture<'a, Result<Box<dyn EthereumNewHeadsConnection>, SourceError>> {
            Box::pin(async move {
                *self.connect_count.lock().expect("connect count lock") += 1;
                let mut failures = self
                    .failures_before_connect
                    .lock()
                    .expect("connection failure counter lock");
                if *failures > 0 {
                    *failures -= 1;
                    return Err(source_error("scripted connection failure", true));
                }
                drop(failures);
                self.connections
                    .lock()
                    .expect("scripted connections lock")
                    .pop_front()
                    .ok_or_else(|| source_error("scripted connection exhausted", false))
            })
        }
    }

    struct ScriptedConnection {
        messages: VecDeque<Result<Option<Vec<u8>>, SourceError>>,
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl EthereumNewHeadsConnection for ScriptedConnection {
        fn send<'a>(&'a mut self, payload: Vec<u8>) -> BoxFuture<'a, Result<(), SourceError>> {
            Box::pin(async move {
                self.sent.lock().expect("sent requests lock").push(payload);
                Ok(())
            })
        }

        fn receive<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<Vec<u8>>, SourceError>> {
            Box::pin(async move { self.messages.pop_front().unwrap_or(Ok(None)) })
        }
    }

    #[test]
    fn disabled_client_never_connects() {
        let connect_count = Arc::new(Mutex::new(0));
        let connector = ScriptedConnector {
            connections: Arc::new(Mutex::new(VecDeque::new())),
            connect_count: Arc::clone(&connect_count),
            failures_before_connect: Arc::new(Mutex::new(0)),
        };
        let client =
            EthereumNewHeadsClient::with_connector(EthereumNewHeadsConfig::disabled(), connector);

        futures_executor::block_on(client.run(|_| true))
            .expect("a disabled wake client must exit successfully");

        assert_eq!(*connect_count.lock().expect("connect count lock"), 0);
    }

    #[test]
    fn config_debug_output_redacts_the_websocket_url() {
        let config = EthereumNewHeadsConfig::new(
            Some("wss://provider-user:provider-secret@example.invalid/ws".to_owned()),
            Duration::from_secs(3),
        )
        .expect("the credential-bearing WebSocket URL must be valid");

        let rendered = format!("{config:?}");

        assert!(rendered.contains("websocket_configured: true"));
        assert!(rendered.contains("reconnect_delay: 3s"));
        assert!(!rendered.contains("provider-user"));
        assert!(!rendered.contains("provider-secret"));
        assert!(!rendered.contains("example.invalid"));
    }

    #[tokio::test]
    async fn reconnects_and_emits_only_a_verified_wake() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let first = ScriptedConnection {
            messages: VecDeque::from([Ok(Some(subscription_confirmation("0xfirst"))), Ok(None)]),
            sent: Arc::clone(&sent),
        };
        let second = ScriptedConnection {
            messages: VecDeque::from([
                Ok(Some(subscription_confirmation("0xsecond"))),
                Ok(Some(notification("0xother", "0x9"))),
                Ok(Some(notification("0xsecond", "0xa"))),
            ]),
            sent: Arc::clone(&sent),
        };
        let connect_count = Arc::new(Mutex::new(0));
        let connector = ScriptedConnector {
            connections: Arc::new(Mutex::new(VecDeque::from([
                Box::new(first) as Box<dyn EthereumNewHeadsConnection>,
                Box::new(second) as Box<dyn EthereumNewHeadsConnection>,
            ]))),
            connect_count: Arc::clone(&connect_count),
            failures_before_connect: Arc::new(Mutex::new(1)),
        };
        let config =
            EthereumNewHeadsConfig::new(Some("ws://example.invalid".to_owned()), Duration::ZERO)
                .expect("the scripted WebSocket URL must be valid");
        let client = EthereumNewHeadsClient::with_connector(config, connector);
        let mut wakes = Vec::new();
        let mut events = Vec::new();

        client
            .run_with_events(
                |wake| {
                    wakes.push(wake);
                    false
                },
                |event| events.push(event),
            )
            .await
            .expect("the scripted wake stream must stop cleanly");

        assert_eq!(*connect_count.lock().expect("connect count lock"), 3);
        assert_eq!(
            wakes,
            vec![EthereumHeadWake {
                announced_height: BlockHeight(10)
            }]
        );
        assert_eq!(
            events,
            vec![
                EthereumNewHeadsConnectionEvent::Failure,
                EthereumNewHeadsConnectionEvent::ReconnectScheduled,
                EthereumNewHeadsConnectionEvent::Connected,
                EthereumNewHeadsConnectionEvent::Disconnected,
                EthereumNewHeadsConnectionEvent::ReconnectScheduled,
                EthereumNewHeadsConnectionEvent::Connected,
                EthereumNewHeadsConnectionEvent::Disconnected,
            ]
        );
        let sent = sent.lock().expect("sent requests lock");
        assert_eq!(sent.len(), 2);
        for request in sent.iter() {
            let request: Value =
                serde_json::from_slice(request).expect("subscription request must be JSON");
            assert_eq!(request["method"], "eth_subscribe");
            assert_eq!(request["params"], serde_json::json!(["newHeads"]));
        }
    }

    #[tokio::test]
    async fn duplicate_old_jump_and_same_height_replacement_are_only_wake_hints() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let connection = ScriptedConnection {
            messages: VecDeque::from([
                Ok(Some(subscription_confirmation("0xsequence"))),
                Ok(Some(notification_with_hash("0xsequence", "0xa", "0x11"))),
                Ok(Some(notification_with_hash("0xsequence", "0xa", "0x11"))),
                Ok(Some(notification_with_hash("0xsequence", "0x8", "0x22"))),
                Ok(Some(notification_with_hash("0xsequence", "0xf", "0x33"))),
                Ok(Some(notification_with_hash("0xsequence", "0xf", "0x44"))),
            ]),
            sent,
        };
        let connector = ScriptedConnector {
            connections: Arc::new(Mutex::new(VecDeque::from([
                Box::new(connection) as Box<dyn EthereumNewHeadsConnection>
            ]))),
            connect_count: Arc::new(Mutex::new(0)),
            failures_before_connect: Arc::new(Mutex::new(0)),
        };
        let config =
            EthereumNewHeadsConfig::new(Some("ws://example.invalid".to_owned()), Duration::ZERO)
                .expect("the scripted WebSocket URL must be valid");
        let client = EthereumNewHeadsClient::with_connector(config, connector);
        let mut wakes = Vec::new();

        client
            .run(|wake| {
                wakes.push(wake);
                wakes.len() < 5
            })
            .await
            .expect("the scripted wake sequence must stop cleanly");

        assert_eq!(
            wakes
                .into_iter()
                .map(|wake| wake.announced_height)
                .collect::<Vec<_>>(),
            vec![
                BlockHeight(10),
                BlockHeight(10),
                BlockHeight(8),
                BlockHeight(15),
                BlockHeight(15),
            ]
        );
        // Neither the announced hash nor ordering becomes canonical evidence;
        // the public hint intentionally exposes only a height for HTTP sync.
    }

    #[test]
    fn subscription_session_requires_confirmation_and_matching_subscription() {
        let mut session = NewHeadsSession::awaiting_confirmation();

        assert_eq!(
            session
                .consume(&subscription_confirmation("0xabc"))
                .expect("valid confirmation must be accepted"),
            None
        );
        assert_eq!(
            session
                .consume(&notification("0xwrong", "0x4"))
                .expect("an unrelated subscription must be ignored"),
            None
        );
        assert_eq!(
            session
                .consume(&notification("0xabc", "0x5"))
                .expect("the matching notification must be accepted"),
            Some(EthereumHeadWake {
                announced_height: BlockHeight(5),
            })
        );
    }

    fn subscription_confirmation(subscription_id: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": SUBSCRIPTION_REQUEST_ID,
            "result": subscription_id,
        }))
        .expect("subscription confirmation fixture must encode")
    }

    fn notification(subscription_id: &str, number: &str) -> Vec<u8> {
        notification_with_hash(subscription_id, number, "0xaaaaaaaa")
    }

    fn notification_with_hash(subscription_id: &str, number: &str, hash: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_subscription",
            "params": {
                "subscription": subscription_id,
                "result": {
                    "number": number,
                    "hash": hash,
                    "parentHash": "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                }
            }
        }))
        .expect("newHeads notification fixture must encode")
    }
}
