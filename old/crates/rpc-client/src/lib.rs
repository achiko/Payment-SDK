//! Protocol-level request dispatch over a replaceable transport.

use transport::Transport;

#[derive(Debug, Clone)]
pub struct RpcClient<T> {
    transport: T,
}

impl<T: Transport> RpcClient<T> {
    #[must_use]
    pub const fn new(transport: T) -> Self {
        Self { transport }
    }

    #[must_use]
    pub fn endpoint(&self) -> &str {
        self.transport.endpoint()
    }

    #[must_use]
    pub fn get_balance(&self) -> &'static str {
        self.transport.send("get_balance")
    }

    #[must_use]
    pub fn send_raw_transaction(&self, envelope: &str) -> &'static str {
        self.transport.send(envelope)
    }
}
