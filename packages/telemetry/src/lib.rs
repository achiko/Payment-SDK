//! Backend-independent observability contract.

use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attribute {
    pub key: String,
    pub value: String,
}

pub trait Telemetry: Send + Sync {
    fn count(&self, name: &'static str, value: u64, attributes: &[Attribute]);

    fn gauge(&self, name: &'static str, value: f64, attributes: &[Attribute]);

    fn duration(&self, name: &'static str, value: Duration, attributes: &[Attribute]);
}
