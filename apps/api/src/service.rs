use std::{error, fmt, future::Future, sync::Arc};

use tokio::sync::watch;

use crate::{
    Clock, Config, ConfigError, DepositObserver, Deposits, Payments, Planner, Sweeps, server,
};

/// A Payment Service with explicitly injected wallet and indexer adapters.
///
/// Construction performs no I/O. This boundary deliberately does not create
/// keys or choose custody: the host must provide a fully composed [`Payments`]
/// value containing its approved wallet and indexer implementations.
pub struct Service {
    config: Config,
    server: http_kit::server::Config,
    payments: Arc<Payments>,
    observers: Vec<Arc<DepositObserver>>,
    deposits: Option<Arc<Deposits>>,
    planner: Option<Arc<Planner>>,
    sweeps: Option<(Arc<Sweeps>, Arc<dyn Clock>)>,
}

impl Service {
    pub fn new(
        config: Config,
        payments: Arc<Payments>,
        server: http_kit::server::Config,
    ) -> Result<Self, ConfigError> {
        config.validate()?;
        server
            .validate()
            .map_err(|error| ConfigError::new(error.to_string()))?;
        if server.bind_addr() != config.bind {
            return Err(ConfigError::new(
                "HTTP security configuration must describe the service listener",
            ));
        }
        if !payments.supports_scopes(&config.scopes) {
            return Err(ConfigError::new(
                "reconciliation scopes must exactly match the injected wallet scopes",
            ));
        }
        Ok(Self {
            config,
            server,
            payments,
            observers: Vec::new(),
            deposits: None,
            planner: None,
            sweeps: None,
        })
    }

    #[must_use]
    pub fn with_observer(mut self, observer: Arc<DepositObserver>) -> Self {
        self.observers.push(observer);
        self
    }

    #[must_use]
    pub fn with_deposits(mut self, deposits: Arc<Deposits>) -> Self {
        self.deposits = Some(deposits);
        self
    }

    #[must_use]
    pub fn with_sweeps(mut self, sweeps: Arc<Sweeps>, clock: Arc<dyn Clock>) -> Self {
        self.sweeps = Some((sweeps, clock));
        self
    }

    #[must_use]
    pub fn with_planner(mut self, planner: Arc<Planner>) -> Self {
        self.planner = Some(planner);
        self
    }

    /// Binds the configured listener and runs until Ctrl+C or task failure.
    pub async fn run(self) -> Result<(), ServiceError> {
        let listener = tokio::net::TcpListener::bind(self.config.bind)
            .await
            .map_err(ServiceError::Io)?;
        self.run_on_signal(listener, async {
            tokio::signal::ctrl_c().await.map_err(ServiceError::Io)
        })
        .await
    }

    /// Binds the configured listener and runs until the supplied signal.
    pub async fn run_until<F>(self, shutdown: F) -> Result<(), ServiceError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let listener = tokio::net::TcpListener::bind(self.config.bind)
            .await
            .map_err(ServiceError::Io)?;
        self.run_on(listener, shutdown).await
    }

    /// Runs on an existing listener, useful for an embedding host and tests.
    pub async fn run_on<F>(
        self,
        listener: tokio::net::TcpListener,
        shutdown: F,
    ) -> Result<(), ServiceError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.run_on_signal(listener, async move {
            shutdown.await;
            Ok(())
        })
        .await
    }

    async fn run_on_signal<F>(
        self,
        listener: tokio::net::TcpListener,
        shutdown: F,
    ) -> Result<(), ServiceError>
    where
        F: Future<Output = Result<(), ServiceError>> + Send + 'static,
    {
        self.validate_observers()?;
        let health = http_kit::server::HealthState::new(false);
        let deposits = self.deposits.clone();
        let app = server::authenticated_gateway(
            self.payments.clone(),
            self.deposits,
            self.planner,
            self.sweeps,
            &self.server,
            health.clone(),
        )
        .map_err(|error| ServiceError::Configuration(error.to_string()))?;
        let (stop, stop_rx) = watch::channel(false);
        let server_stop = stop_rx.clone();
        let mut http = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(wait_for_stop(server_stop))
                .await
        });
        let reconciler = Reconciler {
            payments: self.payments,
            observers: self.observers,
            deposits,
            scopes: self.config.scopes,
            interval: self.config.reconcile_interval,
            limit: self.config.reconcile_limit,
            health: health.clone(),
            stop: stop_rx,
        };
        let mut worker = tokio::spawn(reconciler.run());

        let outcome = tokio::select! {
            signal = shutdown => signal,
            result = &mut http => match result {
                Ok(Ok(())) => Err(ServiceError::Stopped("HTTP server stopped unexpectedly")),
                Ok(Err(error)) => Err(ServiceError::Io(error)),
                Err(error) => Err(ServiceError::Task(error)),
            },
            result = &mut worker => match result {
                Ok(()) => Err(ServiceError::Stopped("reconciliation worker stopped unexpectedly")),
                Err(error) => Err(ServiceError::Task(error)),
            },
        };

        health.set_ready(false);
        let _ = stop.send(true);
        if !http.is_finished() {
            http.await
                .map_err(ServiceError::Task)?
                .map_err(ServiceError::Io)?;
        }
        if !worker.is_finished() {
            worker.await.map_err(ServiceError::Task)?;
        }
        outcome
    }

    fn validate_observers(&self) -> Result<(), ServiceError> {
        for (position, observer) in self.observers.iter().enumerate() {
            if !self.config.scopes.contains(observer.scope()) {
                return Err(ServiceError::Configuration(
                    "deposit observer scope is not configured for this service".to_owned(),
                ));
            }
            if self.observers[..position]
                .iter()
                .any(|existing| existing.scope() == observer.scope())
            {
                return Err(ServiceError::Configuration(
                    "only one deposit observer may own a configured scope".to_owned(),
                ));
            }
        }
        if let Some(deposits) = &self.deposits
            && !self
                .observers
                .iter()
                .any(|observer| observer.scope() == deposits.scope())
        {
            return Err(ServiceError::Configuration(
                "deposit HTTP routes require an observer for the same scope".to_owned(),
            ));
        }
        Ok(())
    }
}

async fn wait_for_stop(mut stop: watch::Receiver<bool>) {
    while !*stop.borrow() && stop.changed().await.is_ok() {}
}

struct Reconciler {
    payments: Arc<Payments>,
    observers: Vec<Arc<DepositObserver>>,
    deposits: Option<Arc<Deposits>>,
    scopes: Vec<indexing::IndexScope>,
    interval: std::time::Duration,
    limit: usize,
    health: http_kit::server::HealthState,
    stop: watch::Receiver<bool>,
}

impl Reconciler {
    async fn run(mut self) {
        loop {
            self.pass().await;
            if self.wait().await {
                return;
            }
        }
    }

    async fn pass(&self) {
        let mut ready = true;
        if let Some(deposits) = &self.deposits
            && deposits.resume(self.limit).await.is_err()
        {
            ready = false;
        }
        for scope in &self.scopes {
            if self
                .payments
                .reconcile(scope.clone(), self.limit)
                .await
                .is_err()
            {
                ready = false;
            }
        }
        let observed_at = unix_timestamp();
        for observer in &self.observers {
            if !self.scopes.contains(observer.scope())
                || observer.pass(self.limit, observed_at).await.is_err()
            {
                ready = false;
            }
        }
        self.health.set_ready(ready);
    }

    async fn wait(&mut self) -> bool {
        tokio::select! {
            () = tokio::time::sleep(self.interval) => false,
            changed = self.stop.changed() => changed.is_err() || *self.stop.borrow(),
        }
    }
}

fn unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[derive(Debug)]
pub enum ServiceError {
    Configuration(String),
    Io(std::io::Error),
    Task(tokio::task::JoinError),
    Stopped(&'static str),
}

impl fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(message) => formatter.write_str(message),
            Self::Io(error) => write!(formatter, "service I/O failed: {error}"),
            Self::Task(error) => write!(formatter, "service task failed: {error}"),
            Self::Stopped(message) => formatter.write_str(message),
        }
    }
}

impl error::Error for ServiceError {}
