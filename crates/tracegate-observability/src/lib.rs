use std::sync::{Arc, Mutex};

use http::HeaderMap;
use opentelemetry::{
    Context, KeyValue,
    global::{self, BoxedTracer},
    trace::TracerProvider as _,
};
use opentelemetry_http::{HeaderExtractor, HeaderInjector};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    Resource,
    propagation::TraceContextPropagator,
    trace::{RandomIdGenerator, Sampler, SdkTracerProvider},
};
use prometheus_client::{
    encoding::{EncodeLabelSet, text::encode},
    metrics::{
        counter::Counter,
        family::{Family, MetricConstructor},
        histogram::Histogram,
    },
    registry::Registry,
};
use thiserror::Error;
use tracegate_core::ObservabilityConfig;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::{EnvFilter, Registry as SubscriberRegistry, layer::SubscriberExt};

#[derive(Debug, Error)]
pub enum ObservabilityError {
    #[error("failed to initialize OTLP trace exporter: {0}")]
    OtlpExporter(String),
    #[error("failed to install tracing subscriber: {0}")]
    Subscriber(String),
}

#[derive(Clone)]
pub struct Telemetry {
    inner: Arc<TelemetryInner>,
}

struct TelemetryInner {
    prometheus_enabled: bool,
    registry: Mutex<Registry>,
    requests: Family<RequestLabels, Counter>,
    request_duration: Family<RequestLabels, Histogram, HistogramConstructor>,
    upstream_errors: Family<UpstreamErrorLabels, Counter>,
    captures: Counter,
    capture_dropped: Counter,
    retention_runs: Counter,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, EncodeLabelSet)]
struct RequestLabels {
    route_id: String,
    method: String,
    status: String,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, EncodeLabelSet)]
struct UpstreamErrorLabels {
    route_id: String,
    method: String,
    status: String,
}

#[derive(Clone)]
struct HistogramConstructor {
    buckets: Vec<f64>,
}

impl MetricConstructor<Histogram> for HistogramConstructor {
    fn new_metric(&self) -> Histogram {
        Histogram::new(self.buckets.iter().copied())
    }
}

#[derive(Clone, Debug)]
pub struct RequestMetric {
    pub route_id: Option<String>,
    pub method: String,
    pub status: u16,
    pub latency_seconds: f64,
    pub upstream_error: bool,
}

pub struct ObservabilityRuntime {
    telemetry: Telemetry,
    tracer_provider: Option<SdkTracerProvider>,
}

impl ObservabilityRuntime {
    pub fn telemetry(&self) -> Telemetry {
        self.telemetry.clone()
    }

    pub fn shutdown(self) {
        if let Some(provider) = self.tracer_provider
            && let Err(err) = provider.shutdown()
        {
            tracing::warn!(error = %err, "failed to shutdown OpenTelemetry provider");
        }
    }
}

impl Telemetry {
    pub fn new(config: &ObservabilityConfig) -> Self {
        let requests = Family::<RequestLabels, Counter>::default();
        let request_duration =
            Family::<RequestLabels, Histogram, HistogramConstructor>::new_with_constructor(
                HistogramConstructor {
                    buckets: vec![
                        0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
                    ],
                },
            );
        let upstream_errors = Family::<UpstreamErrorLabels, Counter>::default();
        let captures = Counter::default();
        let capture_dropped = Counter::default();
        let retention_runs = Counter::default();
        let mut registry = Registry::default();

        registry.register(
            "tracegate_requests",
            "HTTP requests handled by TraceGate.",
            requests.clone(),
        );
        registry.register(
            "tracegate_request_duration_seconds",
            "TraceGate request duration in seconds.",
            request_duration.clone(),
        );
        registry.register(
            "tracegate_upstream_errors",
            "TraceGate upstream error responses and transport failures.",
            upstream_errors.clone(),
        );
        registry.register(
            "tracegate_captures",
            "Request and response captures persisted by TraceGate.",
            captures.clone(),
        );
        registry.register(
            "tracegate_capture_dropped",
            "Capture or request-storage writes dropped without breaking proxy traffic.",
            capture_dropped.clone(),
        );
        registry.register(
            "tracegate_storage_retention_runs",
            "Capture-store retention runs completed by TraceGate.",
            retention_runs.clone(),
        );

        Self {
            inner: Arc::new(TelemetryInner {
                prometheus_enabled: config.prometheus_enabled,
                registry: Mutex::new(registry),
                requests,
                request_duration,
                upstream_errors,
                captures,
                capture_dropped,
                retention_runs,
            }),
        }
    }

    pub fn prometheus_enabled(&self) -> bool {
        self.inner.prometheus_enabled
    }

    pub fn record_request(&self, metric: RequestMetric) {
        let labels = RequestLabels {
            route_id: metric.route_id.clone().unwrap_or_else(|| "none".to_owned()),
            method: metric.method,
            status: metric.status.to_string(),
        };

        self.inner.requests.get_or_create(&labels).inc();
        self.inner
            .request_duration
            .get_or_create(&labels)
            .observe(metric.latency_seconds);

        if metric.upstream_error {
            self.inner
                .upstream_errors
                .get_or_create(&UpstreamErrorLabels {
                    route_id: labels.route_id,
                    method: labels.method,
                    status: labels.status,
                })
                .inc();
        }
    }

    pub fn record_capture(&self) {
        self.inner.captures.inc();
    }

    pub fn record_capture_dropped(&self) {
        self.inner.capture_dropped.inc();
    }

    pub fn record_retention_run(&self) {
        self.inner.retention_runs.inc();
    }

    pub fn render_prometheus(&self) -> Result<String, std::fmt::Error> {
        let registry = self
            .inner
            .registry
            .lock()
            .expect("metrics registry poisoned");
        let mut buffer = String::new();
        encode(&mut buffer, &registry)?;
        Ok(buffer)
    }
}

pub fn init(config: &ObservabilityConfig) -> Result<ObservabilityRuntime, ObservabilityError> {
    global::set_text_map_propagator(TraceContextPropagator::new());
    let telemetry = Telemetry::new(config);
    let tracer_provider = tracer_provider(config)?;

    if let Some(provider) = tracer_provider.clone() {
        if config.json_logs {
            let tracer = provider.tracer(config.service_name.clone());
            let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
            let fmt_layer = tracing_subscriber::fmt::layer()
                .json()
                .flatten_event(true)
                .with_target(true);
            let subscriber = SubscriberRegistry::default()
                .with(env_filter())
                .with(fmt_layer)
                .with(otel_layer);
            tracing::subscriber::set_global_default(subscriber)
                .map_err(|err| ObservabilityError::Subscriber(err.to_string()))?;
        } else {
            let tracer = provider.tracer(config.service_name.clone());
            let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
            let fmt_layer = tracing_subscriber::fmt::layer().with_target(true);
            let subscriber = SubscriberRegistry::default()
                .with(env_filter())
                .with(fmt_layer)
                .with(otel_layer);
            tracing::subscriber::set_global_default(subscriber)
                .map_err(|err| ObservabilityError::Subscriber(err.to_string()))?;
        }
    } else if config.json_logs {
        let fmt_layer = tracing_subscriber::fmt::layer()
            .json()
            .flatten_event(true)
            .with_target(true);
        let subscriber = SubscriberRegistry::default()
            .with(env_filter())
            .with(fmt_layer);
        tracing::subscriber::set_global_default(subscriber)
            .map_err(|err| ObservabilityError::Subscriber(err.to_string()))?;
    } else {
        let fmt_layer = tracing_subscriber::fmt::layer().with_target(true);
        let subscriber = SubscriberRegistry::default()
            .with(env_filter())
            .with(fmt_layer);
        tracing::subscriber::set_global_default(subscriber)
            .map_err(|err| ObservabilityError::Subscriber(err.to_string()))?;
    }

    Ok(ObservabilityRuntime {
        telemetry,
        tracer_provider,
    })
}

pub fn extract_context(headers: &HeaderMap) -> Context {
    global::get_text_map_propagator(|propagator| propagator.extract(&HeaderExtractor(headers)))
}

pub fn inject_context(headers: &mut HeaderMap) {
    global::get_text_map_propagator(|propagator| {
        propagator.inject_context(
            &tracing::Span::current().context(),
            &mut HeaderInjector(headers),
        );
    });

    if !headers.contains_key("traceparent") {
        let _ = headers.insert("traceparent", generated_traceparent().parse().unwrap());
    }
}

pub fn set_span_parent(span: &tracing::Span, parent: Context) {
    let _ = span.set_parent(parent);
}

fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

fn tracer_provider(
    config: &ObservabilityConfig,
) -> Result<Option<SdkTracerProvider>, ObservabilityError> {
    let Some(endpoint) = config.otlp_endpoint.as_deref() else {
        return Ok(None);
    };

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .map_err(|err| ObservabilityError::OtlpExporter(err.to_string()))?;
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_sampler(Sampler::ParentBased(Box::new(Sampler::AlwaysOn)))
        .with_id_generator(RandomIdGenerator::default())
        .with_resource(
            Resource::builder()
                .with_service_name(config.service_name.clone())
                .with_attribute(KeyValue::new(
                    "deployment.environment.name",
                    config.environment.clone(),
                ))
                .build(),
        )
        .build();

    Ok(Some(provider))
}

pub fn trace_id_hex_from_traceparent(value: &str) -> Option<&str> {
    let mut parts = value.split('-');
    let version = parts.next()?;
    let trace_id = parts.next()?;
    let span_id = parts.next()?;
    let flags = parts.next()?;

    if parts.next().is_some()
        || version.len() != 2
        || trace_id.len() != 32
        || span_id.len() != 16
        || flags.len() != 2
        || trace_id == "00000000000000000000000000000000"
        || span_id == "0000000000000000"
        || !version.chars().all(|ch| ch.is_ascii_hexdigit())
        || !trace_id.chars().all(|ch| ch.is_ascii_hexdigit())
        || !span_id.chars().all(|ch| ch.is_ascii_hexdigit())
        || !flags.chars().all(|ch| ch.is_ascii_hexdigit())
    {
        return None;
    }

    Some(trace_id)
}

fn generated_traceparent() -> String {
    let trace_id = uuid::Uuid::now_v7().simple().to_string();
    let span_seed = uuid::Uuid::now_v7().simple().to_string();
    let span_id = &span_seed[..16];
    format!("00-{trace_id}-{span_id}-01")
}

pub fn noop_tracer() -> BoxedTracer {
    global::tracer("tracegate-noop")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(prometheus_enabled: bool) -> ObservabilityConfig {
        ObservabilityConfig {
            service_name: "tracegate-test".to_owned(),
            environment: "test".to_owned(),
            otlp_endpoint: None,
            prometheus_enabled,
            json_logs: true,
        }
    }

    #[test]
    fn metrics_record_requests_and_errors() {
        let telemetry = Telemetry::new(&config(true));

        telemetry.record_request(RequestMetric {
            route_id: Some("payments".to_owned()),
            method: "GET".to_owned(),
            status: 500,
            latency_seconds: 0.042,
            upstream_error: true,
        });

        let metrics = telemetry.render_prometheus().unwrap();
        assert!(metrics.contains("tracegate_requests_total"));
        assert!(metrics.contains("route_id=\"payments\""));
        assert!(metrics.contains("tracegate_request_duration_seconds"));
        assert!(metrics.contains("tracegate_upstream_errors_total"));
    }

    #[test]
    fn metrics_record_capture_and_retention_counts() {
        let telemetry = Telemetry::new(&config(true));

        telemetry.record_capture();
        telemetry.record_capture_dropped();
        telemetry.record_retention_run();

        let metrics = telemetry.render_prometheus().unwrap();
        assert!(metrics.contains("tracegate_captures_total"));
        assert!(metrics.contains("tracegate_capture_dropped_total"));
        assert!(metrics.contains("tracegate_storage_retention_runs_total"));
    }

    #[test]
    fn validates_traceparent_shape_for_proof_queries() {
        assert_eq!(
            trace_id_hex_from_traceparent(
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
            ),
            Some("4bf92f3577b34da6a3ce929d0e0e4736")
        );
        assert!(trace_id_hex_from_traceparent("not-a-traceparent").is_none());
    }
}
