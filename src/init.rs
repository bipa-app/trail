use anyhow::Context;
use opentelemetry_otlp::WithExportConfig;
use std::time::Duration;
use tracing_subscriber::layer::SubscriberExt;

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
) -> anyhow::Result<Handle> {
    let registry = tracing_subscriber::Registry::default()
        .with(tracing_subscriber::EnvFilter::new(rust_log))
        .with(tracing_subscriber::fmt::Layer::default().compact());

    let resource = resource(name, version);

    let logger_provider = logger(otel_endpoint, resource.clone())?;
    let tracer_provider = tracer(otel_endpoint, resource.clone())?;
    let meter_provider = meter(otel_endpoint, resource.clone())?;

    let logger =
        opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&logger_provider);

    tracing::subscriber::set_global_default(registry.with(logger))?;
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
        .with_period(Duration::from_secs(15))
        .with_timeout(Duration::from_secs(5))
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
        .with_batch_config(
            opentelemetry_sdk::trace::BatchConfigBuilder::default()
                .with_max_queue_size(30000)
                .with_max_export_batch_size(10000)
                .with_scheduled_delay(Duration::from_secs(5))
                .build(),
        )
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