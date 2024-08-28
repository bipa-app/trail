use anyhow::Context;
use opentelemetry_otlp::WithExportConfig;
use std::time::Duration;
use tracing_subscriber::{layer::SubscriberExt, registry::Registry};

pub struct Handle;

impl Drop for Handle {
    fn drop(&mut self) {
        opentelemetry::global::shutdown_tracer_provider();
    }
}

pub fn init(
    name: &'static str,
    version: &'static str,
    otel_endpoint: &str,
    rust_log: &str,
    tracing_registry: impl FnOnce(Registry) -> Registry,
) -> anyhow::Result<Handle> {
    let registry = tracing_subscriber::Registry::default()
        .with(tracing_subscriber::EnvFilter::new(rust_log))
        .with(tracing_subscriber::fmt::Layer::default().compact());

    let resource = resource(name, version);

    let logger_provider = logger(otel_endpoint, resource.clone())?;
    let tracer_provider = tracer(otel_endpoint, resource.clone())?;
    let meter_provider = meter(otel_endpoint, resource.clone())?;

    let env_logger = env_logger::Builder::new().parse_filters(rust_log).build();
    let otel_logger = opentelemetry_appender_log::OpenTelemetryLogBridge::new(&logger_provider);

    log::set_max_level(env_logger.filter());
    log::set_boxed_logger(Box::new(Logger(env_logger, otel_logger)))?;

    tracing::subscriber::set_global_default(tracing_registry(registry.with(
        opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&logger_provider),
    )))?;

    opentelemetry::global::set_tracer_provider(tracer_provider);
    opentelemetry::global::set_meter_provider(meter_provider);

    Ok(Handle)
}

fn logger(
    otel_endpoint: &str,
    resource: opentelemetry_sdk::Resource,
) -> anyhow::Result<opentelemetry_sdk::logs::LoggerProvider> {
    opentelemetry_otlp::new_pipeline()
        .logging()
        .with_resource(resource)
        .with_exporter(exporter(otel_endpoint))
        .install_batch(opentelemetry_sdk::runtime::Tokio)
        .context("could not build logs pipeline")
}

fn meter(
    otel_endpoint: &str,
    resource: opentelemetry_sdk::Resource,
) -> anyhow::Result<opentelemetry_sdk::metrics::SdkMeterProvider> {
    opentelemetry_otlp::new_pipeline()
        .metrics(opentelemetry_sdk::runtime::Tokio)
        .with_exporter(exporter(otel_endpoint))
        .with_resource(resource)
        .build()
        .context("could not build metrics pipeline")
}

fn tracer(
    otel_endpoint: &str,
    resource: opentelemetry_sdk::Resource,
) -> anyhow::Result<opentelemetry_sdk::trace::TracerProvider> {
    opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(exporter(otel_endpoint))
        .with_trace_config(opentelemetry_sdk::trace::Config::default().with_resource(resource))
        .install_batch(opentelemetry_sdk::runtime::Tokio)
        .context("could not build tracing pipeline")
}

fn exporter(endpoint: &str) -> opentelemetry_otlp::TonicExporterBuilder {
    opentelemetry_otlp::new_exporter()
        .tonic()
        .with_endpoint(endpoint)
}

fn resource(name: &'static str, version: &'static str) -> opentelemetry_sdk::Resource {
    use opentelemetry_semantic_conventions::resource::{SERVICE_NAME, SERVICE_VERSION};

    opentelemetry_sdk::Resource::new([
        opentelemetry::KeyValue::new(SERVICE_NAME, name),
        opentelemetry::KeyValue::new(SERVICE_VERSION, version),
    ])
}

struct Logger<P, L>(
    env_logger::Logger,
    opentelemetry_appender_log::OpenTelemetryLogBridge<P, L>,
)
where
    P: opentelemetry::logs::LoggerProvider<Logger = L> + Send + Sync,
    L: opentelemetry::logs::Logger + Send + Sync;

impl<P, L> log::Log for Logger<P, L>
where
    P: opentelemetry::logs::LoggerProvider<Logger = L> + Send + Sync,
    L: opentelemetry::logs::Logger + Send + Sync,
{
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        self.0.enabled(metadata) || self.1.enabled(metadata)
    }

    fn log(&self, record: &log::Record<'_>) {
        self.0.log(record);
        self.1.log(record);
    }

    fn flush(&self) {
        self.0.flush();
        self.1.flush();
    }
}
