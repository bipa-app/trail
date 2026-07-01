#[allow(dead_code)] // Someone has to hold the guard oras
pub struct Handle {
    #[cfg(feature = "sentry")]
    sentry: sentry::ClientInitGuard,
    tracer_provider: opentelemetry_sdk::trace::SdkTracerProvider,
    logger_provider: opentelemetry_sdk::logs::SdkLoggerProvider,
    meter_provider: opentelemetry_sdk::metrics::SdkMeterProvider,
}

impl Handle {
    pub fn shutdown(&self) {
        if let Err(e) = self.logger_provider.shutdown() {
            eprintln!("Error during logger shutdown: {:?}", e);
        }
        if let Err(e) = self.tracer_provider.shutdown() {
            eprintln!("Error during tracer shutdown: {:?}", e);
        }
        if let Err(e) = self.meter_provider.shutdown() {
            eprintln!("Error during meter shutdown: {:?}", e);
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.shutdown();
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
    init_with_baggage(
        name,
        version,
        instance,
        otel_endpoint,
        sentry_dsn,
        rust_log,
        [],
    )
}

pub fn init_with_baggage(
    name: &'static str,
    version: &'static str,
    instance: &str,
    otel_endpoint: &str,
    sentry_dsn: &str,
    rust_log: &str,
    baggage_allowlist: impl IntoIterator<Item = opentelemetry::Key>,
) -> anyhow::Result<Handle> {
    let baggage_allowlist = baggage_allowlist.into_iter().collect::<Vec<_>>();

    #[cfg(feature = "sentry")]
    let (sentry, sentry_logger) = sentry(sentry_dsn, version);
    #[cfg(not(feature = "sentry"))]
    let _ = sentry_dsn;

    let logger_provider = logger(otel_endpoint, name, version, instance)?;
    let tracer_provider = tracer(
        otel_endpoint,
        name,
        version,
        instance,
        baggage_allowlist.clone(),
    )?;
    let meter_provider = meter(otel_endpoint, name, version, instance)?;

    let env_logger = env_logger::Builder::new()
        .write_style(env_logger::fmt::WriteStyle::Always)
        .parse_filters(rust_log)
        .build();

    let otel_logger = opentelemetry_appender_log::OpenTelemetryLogBridge::new(&logger_provider);

    log::set_max_level(env_logger.filter());
    log::set_boxed_logger(Box::new(Logger {
        env: env_logger,
        #[cfg(feature = "sentry")]
        sentry: sentry_logger,
        otel: otel_logger,
    }))?;

    install_panic_logger();

    opentelemetry::global::set_text_map_propagator(text_map_propagator(
        !baggage_allowlist.is_empty(),
    ));
    opentelemetry::global::set_tracer_provider(tracer_provider.clone());
    opentelemetry::global::set_meter_provider(meter_provider.clone());

    Ok(Handle {
        #[cfg(feature = "sentry")]
        sentry,
        tracer_provider,
        logger_provider,
        meter_provider,
    })
}

fn text_map_propagator(
    include_baggage: bool,
) -> opentelemetry::propagation::TextMapCompositePropagator {
    use opentelemetry_sdk::propagation::{BaggagePropagator, TraceContextPropagator};

    let mut propagators: Vec<Box<dyn opentelemetry::propagation::TextMapPropagator + Send + Sync>> =
        vec![Box::new(TraceContextPropagator::new())];
    if include_baggage {
        propagators.insert(0, Box::new(BaggagePropagator::new()));
    }

    opentelemetry::propagation::TextMapCompositePropagator::new(propagators)
}

fn logger(
    otel_endpoint: &str,
    name: &'static str,
    version: &'static str,
    instance: &str,
) -> anyhow::Result<opentelemetry_sdk::logs::SdkLoggerProvider> {
    use opentelemetry_otlp::{WithExportConfig, WithTonicConfig};
    use opentelemetry_semantic_conventions::resource::{
        SERVICE_INSTANCE_ID, SERVICE_NAME, SERVICE_VERSION,
    };

    let exporter = opentelemetry_otlp::LogExporter::builder()
        .with_tonic()
        .with_endpoint(otel_endpoint)
        .with_compression(opentelemetry_otlp::Compression::Gzip)
        .build()?;

    Ok(opentelemetry_sdk::logs::SdkLoggerProvider::builder()
        .with_resource(
            opentelemetry_sdk::Resource::builder()
                .with_attributes([
                    opentelemetry::KeyValue::new(SERVICE_NAME, name),
                    opentelemetry::KeyValue::new(SERVICE_VERSION, version),
                    opentelemetry::KeyValue::new(SERVICE_INSTANCE_ID, instance.to_owned()),
                ])
                .build(),
        )
        .with_batch_exporter(exporter)
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
        .with_compression(opentelemetry_otlp::Compression::Gzip)
        .build()?;

    let reader = opentelemetry_sdk::metrics::PeriodicReader::builder(exporter)
        .with_interval(std::time::Duration::from_secs(20))
        .build();

    Ok(opentelemetry_sdk::metrics::SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(
            opentelemetry_sdk::Resource::builder()
                .with_attributes([
                    opentelemetry::KeyValue::new(SERVICE_NAMESPACE, name),
                    opentelemetry::KeyValue::new(SERVICE_NAME, version),
                    opentelemetry::KeyValue::new(SERVICE_INSTANCE_ID, instance.to_owned()),
                ])
                .build(),
        )
        .build())
}

fn tracer(
    otel_endpoint: &str,
    name: &'static str,
    version: &'static str,
    instance: &str,
    baggage_allowlist: impl IntoIterator<Item = opentelemetry::Key>,
) -> anyhow::Result<opentelemetry_sdk::trace::SdkTracerProvider> {
    use opentelemetry_otlp::{WithExportConfig, WithTonicConfig};
    use opentelemetry_semantic_conventions::resource::{
        SERVICE_INSTANCE_ID, SERVICE_NAME, SERVICE_VERSION,
    };

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(otel_endpoint)
        .with_compression(opentelemetry_otlp::Compression::Gzip)
        .build()?;

    let mut builder = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            opentelemetry_sdk::Resource::builder()
                .with_attributes([
                    opentelemetry::KeyValue::new(SERVICE_NAME, name),
                    opentelemetry::KeyValue::new(SERVICE_VERSION, version),
                    opentelemetry::KeyValue::new(SERVICE_INSTANCE_ID, instance.to_owned()),
                ])
                .build(),
        );

    let allowed: std::collections::HashSet<opentelemetry::Key> =
        baggage_allowlist.into_iter().collect();
    if !allowed.is_empty() {
        builder = builder.with_span_processor(crate::BaggageSpanProcessor::new(allowed));
    }

    Ok(builder.build())
}

// Native panic capture, independent of Sentry: a global hook that routes panics
// through `log` (and thus to Loki via the OTLP bridge). Chains the previous hook
// so it composes with Sentry's PanicIntegration while Sentry is enabled, and
// falls back to the default hook once Sentry is dropped.
fn install_panic_logger() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info.location().map(|l| l.to_string()).unwrap_or_default();
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("<non-string panic payload>");
        let backtrace = std::backtrace::Backtrace::capture().to_string();
        log::error!(
            target: "panic",
            kind = "panic",
            panic_location = location.as_str(),
            backtrace = backtrace.as_str();
            "panic: {message}"
        );
        previous(info);
    }));
}

#[cfg(feature = "sentry")]
fn sentry(
    dsn: &str,
    version: &str,
) -> (
    sentry::ClientInitGuard,
    sentry_log::SentryLogger<sentry_log::NoopLogger>,
) {
    let sentry = sentry::init((
        dsn.to_owned(),
        sentry::ClientOptions {
            release: Some(std::borrow::Cow::Owned(String::from(version))),
            ..Default::default()
        }
        .add_integration(sentry::integrations::panic::PanicIntegration::new())
        .add_integration(SentryOtel),
    ));

    (sentry, sentry_log::SentryLogger::new())
}

struct Logger<P, L>
where
    P: opentelemetry::logs::LoggerProvider<Logger = L> + Send + Sync,
    L: opentelemetry::logs::Logger + Send + Sync,
{
    env: env_logger::Logger,
    #[cfg(feature = "sentry")]
    sentry: sentry_log::SentryLogger<sentry_log::NoopLogger>,
    otel: opentelemetry_appender_log::OpenTelemetryLogBridge<P, L>,
}

impl<P, L> log::Log for Logger<P, L>
where
    P: opentelemetry::logs::LoggerProvider<Logger = L> + Send + Sync,
    L: opentelemetry::logs::Logger + Send + Sync,
{
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        let enabled = self.env.enabled(metadata) || self.otel.enabled(metadata);
        #[cfg(feature = "sentry")]
        let enabled = enabled || self.sentry.enabled(metadata);
        enabled
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.env.enabled(record.metadata()) {
            self.env.log(record);
            #[cfg(feature = "sentry")]
            self.sentry.log(record);
            self.otel.log(record);
        }
    }

    fn flush(&self) {
        self.env.flush();
        // sentry's logger flush hits an unimplemented! error, so it is skipped.
        self.otel.flush();
    }
}

#[cfg(feature = "sentry")]
struct SentryOtel;
#[cfg(feature = "sentry")]
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

#[cfg(test)]
mod tests {
    use super::text_map_propagator;
    use opentelemetry::propagation::TextMapPropagator as _;

    #[test]
    fn text_map_propagator_includes_baggage_when_enabled() {
        let propagator = text_map_propagator(true);
        let fields = propagator.fields().collect::<Vec<_>>();

        assert!(fields.contains(&"traceparent"));
        assert!(fields.contains(&"baggage"));
    }

    #[test]
    fn text_map_propagator_omits_baggage_when_disabled() {
        let propagator = text_map_propagator(false);
        let fields = propagator.fields().collect::<Vec<_>>();

        assert!(fields.contains(&"traceparent"));
        assert!(!fields.contains(&"baggage"));
    }
}
