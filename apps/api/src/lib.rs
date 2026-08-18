//! Durable payment orchestration composed over protocol-neutral wallets.

mod allocation;
mod collection;
mod composition;
mod config;
mod deposit;
mod deposit_http;
mod deposit_ledger;
mod observation;
mod payment;
mod planner;
mod resolver;
mod server;
mod service;
mod startup;
mod store;
mod sweep_http;
mod token;

pub use collection::{CollectionStore, DepositWallets, Sweeps};
pub use composition::Runtime;
pub use config::KeyConfig;
pub use config::{
    BitcoinConfig, Config, ConfigError, DepositConfig, EthereumAsset, EthereumConfig,
    IndexerConfig, RuntimeConfig, ServerConfig, WalletConfig,
};
pub use deposit::{DepositStore, Deposits};
pub use deposit_http::{
    AddressResponse, AssetRequest, AssetResponse, DepositFilter, DepositList, DepositRequest,
    DepositResponse, ResumeRequest, ResumeResponse, StateQuery, StateResponse, deposit_routes,
};
pub use deposit_ledger::{
    BalanceResponse, BalancesResponse, BlockResponse, CauseResponse, EntryResponse, HistoryFilter,
    HistoryResponse, ProofResponse, StatusResponse,
};
pub use deposits::{
    Deposit, DepositError, DepositId, DepositPage, DepositQuery, DepositRegistration,
};
pub use observation::{DepositObserver, ObservationStore, ObserveError, Pass as ObservationPass};
pub use payment::{
    Error, ErrorKind, Evidence, EvidenceStatus, Payment, Payments, Request, Scope, Stage, Watch,
};
pub use planner::{CollectionPolicy, PlanRequest, PlanStore, Planner};
pub use server::{LIVE_PATH, READY_PATH, authenticated_gateway, gateway_router, router, serve};
pub use service::{Service, ServiceError};
pub use startup::{CompositionError, Secrets};
pub use store::{
    ReconcileBatch, ReconcileState, ReconcileStore, Repository, Storage, StorageRepository,
    StoredCursor, StoredPayment,
};
pub use sweep_http::{
    Clock, SweepAddress, SweepAllocation, SweepAsset, SweepLeg, SweepResponse, SystemClock,
    plan_routes, sweep_routes,
};
pub use token::GasWallet;
