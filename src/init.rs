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
        .write_style(env_logger::fmt::WriteStyle::Auto)
        .parse_filters(rust_log)
        .build();

    let otel_logger = opentelemetry_appender_log::OpenTelemetryLogBridge::new(&logger_provider);

    log::set_max_level(env_logger.filter());
    log::set_boxed_logger(Box::new(Logger {
        env: env_logger,
        panic_stderr: panic_stderr(),
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

// Log target for the native panic hook. `Logger::log` keys off it to keep the
// panic out of the Sentry sink (Sentry's PanicIntegration already captures it) and
// to print it on stderr whatever RUST_LOG says.
pub(crate) const PANIC_TARGET: &str = "panic";

fn panic_stderr() -> env_logger::Logger {
    env_logger::Builder::new()
        .write_style(env_logger::fmt::WriteStyle::Auto)
        .filter_level(log::LevelFilter::Error)
        .build()
}

// Native panic capture, independent of Sentry: a global hook that routes panics
// through `log`, to stderr as one line and to Loki via the OTLP bridge. It replaces
// the hook it finds instead of chaining it: std's default printer would add the
// whole multi-line trace, and Sentry's PanicIntegration hook chains that printer.
// With Sentry enabled, its handler is called directly instead.
fn install_panic_logger() {
    // Install exactly once so repeated init calls don't stack panic hooks.
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            let location = info.location().map(|l| l.to_string()).unwrap_or_default();
            let message = info
                .payload()
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
                .unwrap_or("<non-string panic payload>");
            let thread = std::thread::current();
            let thread = thread.name().unwrap_or("<unnamed>");
            // force_capture: panic diagnostics must not depend on RUST_BACKTRACE being
            // set. The logger reduces it to `error.origin` and prints no more.
            let backtrace = std::backtrace::Backtrace::force_capture().to_string();
            if log::max_level() == log::LevelFilter::Off {
                // RUST_LOG=off disables the logger, not the report of a panic. A failed
                // write is ignored: panicking here would abort.
                use std::io::Write as _;
                let _ = writeln!(
                    std::io::stderr(),
                    "panic: {} thread={} panic_location={} error.origin={}",
                    crate::compact::single_line(message, 1024),
                    crate::compact::single_line(thread, 128),
                    crate::compact::single_line(&location, 256),
                    crate::compact::backtrace_origin(&backtrace).unwrap_or_default(),
                );
            } else {
                // signature::compute classifies panic records by this panic_location kv.
                log::error!(
                    target: PANIC_TARGET,
                    kind = "panic",
                    thread = thread,
                    panic_location = location.as_str(),
                    backtrace = backtrace.as_str();
                    "panic: {message}"
                );
            }
            #[cfg(feature = "sentry")]
            sentry::integrations::panic::panic_handler(info);
        }));
    });
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
        sentry::ClientOptions::new()
            .release(std::borrow::Cow::Owned(String::from(version)))
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
    panic_stderr: env_logger::Logger,
    #[cfg(feature = "sentry")]
    sentry: sentry_log::SentryLogger<sentry_log::NoopLogger>,
    otel: opentelemetry_appender_log::OpenTelemetryLogBridge<P, L>,
}

impl<P, L> Logger<P, L>
where
    P: opentelemetry::logs::LoggerProvider<Logger = L> + Send + Sync,
    L: opentelemetry::logs::Logger + Send + Sync,
{
    // Forward to each sink on its own filter: env_logger's per-target RUST_LOG
    // must not gate the OTLP/Loki export, or panic/error records whose target
    // isn't in RUST_LOG (e.g. target "panic") would be dropped before Loki.
    fn dispatch(&self, record: &log::Record<'_>) {
        use log::Log as _;

        if record.target() == PANIC_TARGET {
            self.panic_stderr.log(record);
        } else if self.env.enabled(record.metadata()) {
            self.env.log(record);
        }
        // The panic hook hands panics to Sentry's handler itself (with a real
        // backtrace); capturing the "panic" log record here too would emit a second,
        // lower-fidelity Sentry event per panic. Loki/stderr still get it.
        #[cfg(feature = "sentry")]
        if record.target() != PANIC_TARGET && self.sentry.enabled(record.metadata()) {
            self.sentry.log(record);
        }
        if self.otel.enabled(record.metadata()) {
            self.otel.log(record);
        }
    }
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
        // Identity comes from the record as written, before compaction rewrites it, so
        // fingerprints stay what they were; only its printed scope and root are made
        // one bounded line. The guards shield ordinary logging from a bug here or a
        // panicking formatter in the record. They cannot help on the panic-hook path
        // (a panic while the hook runs aborts before unwinding), so this code is also
        // written to be panic-free.
        let mut sig = if record.level() == log::Level::Error {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::signature::compute(record)
            }))
            .unwrap_or_default()
        } else {
            crate::signature::Signature::default()
        };
        sig.scope = crate::compact::single_line(&sig.scope, crate::compact::SCOPE_MAX).into_owned();
        sig.root = crate::compact::single_line(&sig.root, crate::compact::ROOT_MAX).into_owned();

        if crate::compact::fits(record) {
            let kvs = crate::signature::WithSignature::new(record.key_values(), &sig);
            return self.dispatch(&record.to_builder().key_values(&kvs).build());
        }
        let compact = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::compact::Record::new(record)
        }));
        // Sinks print the target and export the module path and file: one bounded line
        // each, like the rest of the record.
        fn meta(m: &str) -> std::borrow::Cow<'_, str> {
            crate::compact::single_line(m, crate::compact::METADATA_MAX)
        }
        let target = meta(record.target());
        let module = record.module_path().map(meta);
        let file = record.file().map(meta);
        let builder = || {
            let mut builder = record.to_builder();
            builder
                .target(&target)
                .module_path(module.as_deref())
                .file(file.as_deref());
            builder
        };
        let Ok(compact) = compact else {
            // Not the original pairs or interpolated body: a formatter in them is what
            // may have panicked. A literal body can't, so it is kept.
            let none: &[(&str, log::kv::Value<'_>)] = &[];
            let kvs = crate::signature::WithSignature::new(&none, &sig);
            let body = match record.args().as_str() {
                Some(literal) => crate::compact::single_line(literal, 1024),
                None => "log record dropped: rewriting it to one line panicked".into(),
            };
            return self.dispatch(
                &builder()
                    .args(format_args!("{body}"))
                    .key_values(&kvs)
                    .build(),
            );
        };
        let kvs = crate::signature::WithSignature::new(&compact, &sig);
        match compact.body.as_deref() {
            Some(body) => self.dispatch(
                &builder()
                    .args(format_args!("{body}"))
                    .key_values(&kvs)
                    .build(),
            ),
            None => self.dispatch(&builder().key_values(&kvs).build()),
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
    use super::{install_panic_logger, text_map_propagator, PANIC_TARGET};
    use opentelemetry::propagation::TextMapPropagator as _;

    // Pins the hook ↔ signature contract end-to-end: the kv names the real hook
    // emits (panic_location etc.) must classify the record as a panic in compute.
    #[test]
    fn panic_hook_record_reaches_compute_with_a_panic_scope() {
        static CAPTURED: std::sync::Mutex<Option<crate::signature::Signature>> =
            std::sync::Mutex::new(None);

        struct Capture;
        impl log::Log for Capture {
            fn enabled(&self, _: &log::Metadata<'_>) -> bool {
                true
            }
            fn log(&self, record: &log::Record<'_>) {
                // Only this test's panic: other tests may panic while the hook is set.
                if record.target() == PANIC_TARGET && record.args().to_string() == "panic: boom" {
                    *CAPTURED.lock().unwrap() = Some(crate::signature::compute(record));
                }
            }
            fn flush(&self) {}
        }

        log::set_boxed_logger(Box::new(Capture)).expect("no other test sets a logger");
        log::set_max_level(log::LevelFilter::Error);
        // silence the default stderr printer while this test runs
        std::panic::set_hook(Box::new(|_| {}));
        install_panic_logger();

        let _ = std::thread::spawn(|| panic!("boom")).join();

        // restore the default printer so later failing tests keep their panic output
        drop(std::panic::take_hook());

        let sig = CAPTURED
            .lock()
            .unwrap()
            .clone()
            .expect("hook logged no record");
        assert_eq!(sig.scope, file!());
        assert_eq!(sig.root, "panic: boom");
        assert!(!sig.fingerprint.is_empty());
    }

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

    // The real logger, end to end: stderr through a pipe, OTLP through the SDK's
    // in-memory exporter, both fed one error record carrying an anyhow-shaped chain
    // and backtrace (as `e:?` renders it in production).
    #[test]
    fn an_error_with_a_backtrace_reaches_every_sink_as_one_bounded_line() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Pipe(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Pipe {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().write(buf)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        struct Chain;
        impl std::fmt::Debug for Chain {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("balances\n\nCaused by:\n    0: failed to get prices for BTC\n    1: couldn't find global balance snapshots\n\nStack backtrace:\n   0: <core::option::Option<T> as anyhow::Context<T, E>>::context::<&str>\n   1: bipa::features::liquid_quote_parts::assemble_price_bag::{closure#0}\n   2: <opentelemetry::context::future_ext::WithContext<F> as core::future::future::Future>::poll\n   3: <bipa::grpc::Server as bipa_grpc_proto::mobile::mobile_server::Mobile>::get_card_config::{closure#0}\n   4: <tokio::runtime::scheduler::multi_thread::worker::Context>::run_task")
            }
        }

        let stderr = Pipe::default();
        let exporter = opentelemetry_sdk::logs::InMemoryLogExporter::default();
        let provider = opentelemetry_sdk::logs::SdkLoggerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let logger = super::Logger {
            env: env_logger::Builder::new()
                .filter_level(log::LevelFilter::Info)
                .write_style(env_logger::fmt::WriteStyle::Never)
                .target(env_logger::Target::Pipe(Box::new(stderr.clone())))
                .build(),
            panic_stderr: super::panic_stderr(),
            #[cfg(feature = "sentry")]
            sentry: sentry_log::SentryLogger::new(),
            otel: opentelemetry_appender_log::OpenTelemetryLogBridge::new(&provider),
        };

        let chain = Chain;
        let kvs = [
            ("user_id", log::kv::Value::from(42)),
            ("e", log::kv::Value::from_debug(&chain)),
        ];
        let record = log::Record::builder()
            .level(log::Level::Error)
            .target("bipa::grpc::balances")
            .module_path(Some("bipa::grpc::balances"))
            .args(format_args!("balances failed"))
            .key_values(&kvs)
            .build();
        let expected = crate::signature::compute(&record);
        log::Log::log(&logger, &record);

        let printed = String::from_utf8(stderr.0.lock().unwrap().clone()).unwrap();
        assert_eq!(printed.matches('\n').count(), 1, "{printed}");
        assert!(!printed.contains("Stack backtrace"), "{printed}");
        assert!(
            printed.contains(
                "e=balances: failed to get prices for BTC: couldn't find global balance snapshots"
            ),
            "{printed}"
        );
        assert!(printed.contains("error.origin=bipa::features::liquid_quote_parts::assemble_price_bag <- bipa::grpc::Server::get_card_config"), "{printed}");
        // Everything after env_logger's `[timestamp LEVEL target] ` header.
        let (_, emitted) = printed.split_once("] ").unwrap();
        assert!(
            emitted.trim_end().len() <= crate::compact::RECORD_MAX,
            "{}",
            emitted.len()
        );

        let logs = exporter.get_emitted_logs().unwrap();
        let [log] = logs.as_slice() else {
            panic!("expected one exported record, got {}", logs.len());
        };
        let attribute = |key: &str| {
            log.record
                .attributes_iter()
                .find(|(k, _)| k.as_str() == key)
                .map(|(_, v)| match v {
                    opentelemetry::logs::AnyValue::String(s) => s.as_str().to_owned(),
                    other => format!("{other:?}"),
                })
        };
        assert_eq!(
            attribute("e").as_deref(),
            Some("balances: failed to get prices for BTC: couldn't find global balance snapshots")
        );
        assert_eq!(
            attribute("error.origin").as_deref(),
            Some("bipa::features::liquid_quote_parts::assemble_price_bag <- bipa::grpc::Server::get_card_config")
        );
        // Fingerprint and root are computed before compaction, so they don't move.
        assert_eq!(attribute("error.fingerprint"), Some(expected.fingerprint));
        assert_eq!(attribute("error.root"), Some(expected.root));
        let attributes: Vec<String> = log
            .record
            .attributes_iter()
            .map(|(_, v)| format!("{v:?}"))
            .collect();
        assert!(
            attributes.iter().all(|v| !v.contains("\\n")),
            "{attributes:?}"
        );
        // Numbers stay typed through the OTLP bridge.
        assert!(log
            .record
            .attributes_iter()
            .any(|(k, v)| k.as_str() == "user_id"
                && matches!(v, opentelemetry::logs::AnyValue::Int(42))));
    }

    // A record whose metadata is hostile: the printed line and the exported `code.*`
    // attributes are still one bounded line each.
    #[test]
    fn hostile_metadata_is_cleaned_and_bounded() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Pipe(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Pipe {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().write(buf)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let stderr = Pipe::default();
        let exporter = opentelemetry_sdk::logs::InMemoryLogExporter::default();
        let provider = opentelemetry_sdk::logs::SdkLoggerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let logger = super::Logger {
            env: env_logger::Builder::new()
                .filter_level(log::LevelFilter::Info)
                .write_style(env_logger::fmt::WriteStyle::Never)
                .target(env_logger::Target::Pipe(Box::new(stderr.clone())))
                .build(),
            panic_stderr: super::panic_stderr(),
            #[cfg(feature = "sentry")]
            sentry: sentry_log::SentryLogger::new(),
            otel: opentelemetry_appender_log::OpenTelemetryLogBridge::new(&provider),
        };

        let target = format!("{}\nFORGED", "t".repeat(5_000));
        let module = "m".repeat(3_000);
        let file = "f".repeat(3_000);
        let big = "v".repeat(10_000);
        let kvs: Vec<(String, String)> = (0..40).map(|i| (format!("k{i}"), big.clone())).collect();
        let kvs: Vec<(&str, log::kv::Value<'_>)> = kvs
            .iter()
            .map(|(k, v)| (k.as_str(), log::kv::Value::from(v.as_str())))
            .collect();
        log::Log::log(
            &logger,
            &log::Record::builder()
                .level(log::Level::Error)
                .target(&target)
                .module_path(Some(&module))
                .file(Some(&file))
                .line(Some(7))
                .args(format_args!("{big}"))
                .key_values(&kvs)
                .build(),
        );

        let printed = String::from_utf8(stderr.0.lock().unwrap().clone()).unwrap();
        assert_eq!(printed.matches('\n').count(), 1, "{printed}");
        // env_logger's own `[timestamp LEVEL ` prefix is the only part outside the budget.
        assert!(
            printed.len() <= crate::compact::RECORD_MAX + 40,
            "{}",
            printed.len()
        );

        let logs = exporter.get_emitted_logs().unwrap();
        let [log] = logs.as_slice() else {
            panic!("expected one exported record, got {}", logs.len());
        };
        for (key, value) in log.record.attributes_iter() {
            assert!(!format!("{value:?}").contains("\\n"), "{key:?}");
            if let (true, opentelemetry::logs::AnyValue::String(s)) =
                (key.as_str().starts_with("code."), value)
            {
                assert!(s.as_str().len() <= crate::compact::METADATA_MAX, "{key:?}");
            }
        }
    }

    // A value whose formatter panics: the rewrite is abandoned, the literal body kept.
    #[test]
    fn a_panicking_value_keeps_the_literal_body() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Pipe(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Pipe {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().write(buf)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        struct Bad;
        impl std::fmt::Display for Bad {
            fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                panic!("formatter panicked")
            }
        }

        let stderr = Pipe::default();
        let provider = opentelemetry_sdk::logs::SdkLoggerProvider::builder().build();
        let logger = super::Logger {
            env: env_logger::Builder::new()
                .filter_level(log::LevelFilter::Info)
                .write_style(env_logger::fmt::WriteStyle::Never)
                .target(env_logger::Target::Pipe(Box::new(stderr.clone())))
                .build(),
            panic_stderr: super::panic_stderr(),
            #[cfg(feature = "sentry")]
            sentry: sentry_log::SentryLogger::new(),
            otel: opentelemetry_appender_log::OpenTelemetryLogBridge::new(&provider),
        };
        let bad = Bad;
        let kvs = [("v", log::kv::Value::from_display(&bad))];
        let record = log::Record::builder()
            .level(log::Level::Warn)
            .target("bipa::x")
            .args(format_args!("payout refused"))
            .key_values(&kvs)
            .build();
        // The panic this provokes prints through whatever hook is installed; nothing
        // here touches the global hook, which other tests rely on.
        log::Log::log(&logger, &record);

        let printed = String::from_utf8(stderr.0.lock().unwrap().clone()).unwrap();
        assert!(printed.contains("payout refused"), "{printed}");
        assert!(!printed.contains("v="), "{printed}");
    }
}
