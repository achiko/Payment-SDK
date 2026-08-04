//! WS composition root: stateless chain operations. It intentionally has no storage dependency.
//!
//! Deployments construct `wallet_worker::WalletService` with authenticated
//! transport, concrete chain RPC clients, and a custody backend. Those choices
//! are deployment configuration and are intentionally not hard-coded here.

fn main() {}
