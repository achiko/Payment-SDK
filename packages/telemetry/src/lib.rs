//! Backend-independent observability contract and Prometheus recorder adapter.

use std::{error::Error, fmt, time::Duration};

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

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

#[derive(Clone, Copy, Debug, Default)]
pub struct NoopTelemetry;

impl Telemetry for NoopTelemetry {
    fn count(&self, _name: &'static str, _value: u64, _attributes: &[Attribute]) {}

    fn gauge(&self, _name: &'static str, _value: f64, _attributes: &[Attribute]) {}

    fn duration(&self, _name: &'static str, _value: Duration, _attributes: &[Attribute]) {}
}

#[derive(Clone, Debug)]
pub struct PrometheusTelemetry {
    handle: PrometheusHandle,
}

impl PrometheusTelemetry {
    /// Installs one process-global recorder and returns its render handle.
    ///
    /// Applications should call this exactly once during composition. The
    /// returned error is structured and does not include configuration secrets.
    pub fn install() -> Result<Self, TelemetryBootstrapError> {
        let handle = PrometheusBuilder::new()
            .install_recorder()
            .map_err(|error| {
                TelemetryBootstrapError::with_source(
                    TelemetryBootstrapErrorKind::RecorderInstallation,
                    "failed to install Prometheus metrics recorder",
                    error,
                )
            })?;
        Ok(Self { handle })
    }

    #[must_use]
    pub fn from_handle(handle: PrometheusHandle) -> Self {
        Self { handle }
    }

    /// Produces a Prometheus text exposition payload for an application-owned route.
    #[must_use]
    pub fn render(&self) -> String {
        self.handle.render()
    }

    /// Removes expired metrics according to recorder configuration.
    pub fn run_upkeep(&self) {
        self.handle.run_upkeep();
    }
}

impl Telemetry for PrometheusTelemetry {
    fn count(&self, name: &'static str, value: u64, attributes: &[Attribute]) {
        let labels = labels(attributes);
        metrics::counter!(name, &labels).increment(value);
    }

    fn gauge(&self, name: &'static str, value: f64, attributes: &[Attribute]) {
        let labels = labels(attributes);
        metrics::gauge!(name, &labels).set(value);
    }

    fn duration(&self, name: &'static str, value: Duration, attributes: &[Attribute]) {
        let labels = labels(attributes);
        metrics::histogram!(name, &labels).record(value.as_secs_f64());
    }
}

fn labels(attributes: &[Attribute]) -> Vec<(String, String)> {
    attributes
        .iter()
        .map(|attribute| (attribute.key.clone(), attribute.value.clone()))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TelemetryBootstrapErrorKind {
    RecorderInstallation,
}

#[derive(Debug)]
pub struct TelemetryBootstrapError {
    pub kind: TelemetryBootstrapErrorKind,
    pub message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl TelemetryBootstrapError {
    fn with_source(
        kind: TelemetryBootstrapErrorKind,
        message: impl Into<String>,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

impl fmt::Display for TelemetryBootstrapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for TelemetryBootstrapError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prometheus_adapter_records_all_metric_kinds_with_labels() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let telemetry = PrometheusTelemetry::from_handle(recorder.handle());
        metrics::with_local_recorder(&recorder, || {
            let attributes = [Attribute {
                key: "scope".to_owned(),
                value: "test".to_owned(),
            }];
            telemetry.count("ix_blocks_total", 2, &attributes);
            telemetry.gauge("ix_height", 42.0, &attributes);
            telemetry.duration("ix_rpc_seconds", Duration::from_millis(250), &attributes);
        });

        let rendered = telemetry.render();
        assert!(rendered.contains("ix_blocks_total{scope=\"test\"} 2"));
        assert!(rendered.contains("ix_height{scope=\"test\"} 42"));
        assert!(rendered.contains("ix_rpc_seconds"));
    }

    #[test]
    fn noop_adapter_accepts_metrics_without_side_effects() {
        let telemetry = NoopTelemetry;
        telemetry.count("test_counter", 1, &[]);
        telemetry.gauge("test_gauge", 1.0, &[]);
        telemetry.duration("test_duration", Duration::from_secs(1), &[]);
    }
}
