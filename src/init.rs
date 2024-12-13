#[allow(dead_code)] // Someone has to hold the guard oras
pub struct Handle(sentry::ClientInitGuard);

impl Drop for Handle {
    fn drop(&mut self) {
        opentelemetry::global::shutdown_tracer_provider();
    }
}

pub fn init(
    name: &'static str,
    version: &'static str,
    instance: &str,
    otel_endpoint: &str,
    sentry_dsn: &str,
    rust_log: &str,
) -> anyhow::Result<Handle> {
    use opentelemetry_sdk::propagation::TraceContextPropagator;

    let (guard, sentry_logger) = sentry(sentry_dsn, version);
    let logger_provider = logger(otel_endpoint, name, version, instance)?;
    let tracer_provider = tracer(otel_endpoint, name, version, instance)?;
    let meter_provider = meter(otel_endpoint, name, version, instance)?;

    let env_logger = env_logger::Builder::new()
        .write_style(env_logger::fmt::WriteStyle::Always)
        .parse_filters(rust_log)
        .build();

    let otel_logger = opentelemetry_appender_log::OpenTelemetryLogBridge::new(&logger_provider);

    log::set_max_level(env_logger.filter());
    log::set_boxed_logger(Box::new(Logger(env_logger, sentry_logger, otel_logger)))?;

    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    opentelemetry::global::set_tracer_provider(tracer_provider);
    opentelemetry::global::set_meter_provider(meter_provider);

    Ok(Handle(guard))
}

fn logger(
    otel_endpoint: &str,
    name: &'static str,
    version: &'static str,
    instance: &str,
) -> anyhow::Result<opentelemetry_sdk::logs::LoggerProvider> {
    use opentelemetry_otlp::{WithExportConfig, WithTonicConfig};
    use opentelemetry_semantic_conventions::resource::{
        SERVICE_INSTANCE_ID, SERVICE_NAME, SERVICE_VERSION,
    };

    let exporter = opentelemetry_otlp::LogExporter::builder()
        .with_tonic()
        .with_endpoint(otel_endpoint)
        .with_compression(opentelemetry_otlp::Compression::Zstd)
        .build()?;

    Ok(opentelemetry_sdk::logs::LoggerProvider::builder()
        .with_resource(opentelemetry_sdk::Resource::new([
            opentelemetry::KeyValue::new(SERVICE_NAME, name),
            opentelemetry::KeyValue::new(SERVICE_VERSION, version),
            opentelemetry::KeyValue::new(SERVICE_INSTANCE_ID, instance.to_string()),
        ]))
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .build())
}

fn meter(
    otel_endpoint: &str,
    name: &'static str,
    version: &'static str,
    instance: &str,
) -> anyhow::Result<opentelemetry_sdk::metrics::SdkMeterProvider> {
    use opentelemetry_otlp::{WithExportConfig, WithTonicConfig};
    use opentelemetry_semantic_conventions::resource::{
        SERVICE_INSTANCE_ID, SERVICE_NAME, SERVICE_NAMESPACE,
    };

    let exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_tonic()
        .with_endpoint(otel_endpoint)
        .with_compression(opentelemetry_otlp::Compression::Zstd)
        .build()?;

    let reader = opentelemetry_sdk::metrics::PeriodicReader::builder(
        exporter,
        opentelemetry_sdk::runtime::Tokio,
    )
    .with_interval(std::time::Duration::from_secs(20))
    .with_timeout(std::time::Duration::from_secs(10))
    .build();

    Ok(opentelemetry_sdk::metrics::SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(opentelemetry_sdk::Resource::new([
            opentelemetry::KeyValue::new(SERVICE_NAMESPACE, name),
            opentelemetry::KeyValue::new(SERVICE_NAME, version),
            opentelemetry::KeyValue::new(SERVICE_INSTANCE_ID, instance.to_string()),
        ]))
        .build())
}

fn tracer(
    otel_endpoint: &str,
    name: &'static str,
    version: &'static str,
    instance: &str,
) -> anyhow::Result<opentelemetry_sdk::trace::TracerProvider> {
    use opentelemetry_otlp::{WithExportConfig, WithTonicConfig};
    use opentelemetry_semantic_conventions::resource::{
        SERVICE_INSTANCE_ID, SERVICE_NAME, SERVICE_VERSION,
    };

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(otel_endpoint)
        .with_compression(opentelemetry_otlp::Compression::Zstd)
        .build()?;

    Ok(opentelemetry_sdk::trace::TracerProvider::builder()
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .with_resource(opentelemetry_sdk::Resource::new([
            opentelemetry::KeyValue::new(SERVICE_NAME, name),
            opentelemetry::KeyValue::new(SERVICE_VERSION, version),
            opentelemetry::KeyValue::new(SERVICE_INSTANCE_ID, instance.to_string()),
        ]))
        .build())
}

fn sentry(
    dsn: &str,
    version: &str,
) -> (
    sentry::ClientInitGuard,
    sentry_log::SentryLogger<sentry_log::NoopLogger>,
) {
    let sentry = sentry::init((
        dsn.to_string(),
        sentry::ClientOptions {
            release: Some(std::borrow::Cow::Owned(String::from(version))),
            ..Default::default()
        }
        .add_integration(sentry::integrations::panic::PanicIntegration::new())
        .add_integration(SentryOtel),
    ));

    (sentry, sentry_log::SentryLogger::new())
}

struct Logger<P, L>(
    env_logger::Logger,
    sentry_log::SentryLogger<sentry_log::NoopLogger>,
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
        self.0.enabled(metadata) || self.1.enabled(metadata) || self.2.enabled(metadata)
    }

    fn log(&self, record: &log::Record<'_>) {
        self.0.log(record);
        self.1.log(record);
        self.2.log(record);
    }

    fn flush(&self) {
        self.0.flush();
        self.1.flush();
        self.2.flush();
    }
}

struct SentryOtel;
impl sentry::Integration for SentryOtel {
    fn process_event(
        &self,
        mut event: sentry::protocol::Event<'static>,
        _: &sentry::ClientOptions,
    ) -> Option<sentry::protocol::Event<'static>> {
        use opentelemetry::trace::TraceContextExt;

        event.tags.insert(
            String::from("otel-trace-id"),
            opentelemetry::Context::current()
                .span()
                .span_context()
                .trace_id()
                .to_string(),
        );

        Some(event)
    }
}
