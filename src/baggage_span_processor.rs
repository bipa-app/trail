use opentelemetry::baggage::BaggageExt;
use opentelemetry::trace::Span as _;
use opentelemetry::{Context, Key, KeyValue, Value};
use opentelemetry_sdk::trace::{Span, SpanProcessor};
use std::collections::HashSet;

/// A [`SpanProcessor`] that copies allowlisted [baggage] entries into span
/// attributes on start.
///
/// OTel baggage is propagated across service boundaries but is **not**
/// automatically attached to spans. This processor bridges that gap: it reads
/// baggage from the parent [`Context`] during `on_start` and sets matching
/// entries as span attributes.
///
/// Only keys present in the configured allowlist are copied — this prevents
/// accidentally leaking sensitive baggage into traces.
///
/// [baggage]: https://opentelemetry.io/docs/concepts/signals/baggage/
#[derive(Debug)]
pub struct BaggageSpanProcessor {
    allowed_keys: HashSet<Key>,
}

impl BaggageSpanProcessor {
    /// Create a new processor that will copy the given baggage keys into span
    /// attributes.
    ///
    /// ```rust,ignore
    /// use opentelemetry::Key;
    ///
    /// let processor = BaggageSpanProcessor::new([
    ///     Key::from_static_str("user.id"),
    ///     Key::from_static_str("tenant.id"),
    /// ]);
    /// ```
    pub fn new(allowed_keys: impl IntoIterator<Item = Key>) -> Self {
        Self {
            allowed_keys: allowed_keys.into_iter().collect(),
        }
    }
}

impl SpanProcessor for BaggageSpanProcessor {
    fn on_start(&self, span: &mut Span, cx: &Context) {
        let baggage = cx.baggage();
        for (key, (value, _metadata)) in baggage.iter() {
            if self.allowed_keys.contains(key) {
                span.set_attribute(KeyValue::new(key.clone(), Value::String(value.clone())));
            }
        }
    }

    fn on_end(&self, _span: opentelemetry_sdk::trace::SpanData) {}

    fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(
        &self,
        _timeout: std::time::Duration,
    ) -> opentelemetry_sdk::error::OTelSdkResult {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{Tracer, TracerProvider};

    fn build_span() -> Span {
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
        let tracer = provider.tracer("test");
        tracer.build(opentelemetry::trace::SpanBuilder::from_name("test"))
    }

    #[test]
    fn copies_allowlisted_keys_only() {
        let processor = BaggageSpanProcessor::new([
            Key::from_static_str("user.id"),
            Key::from_static_str("tenant.id"),
        ]);

        let cx = Context::new().with_baggage([
            KeyValue::new("user.id", "42"),
            KeyValue::new("tenant.id", "acme"),
            KeyValue::new("secret", "should-not-appear"),
        ]);

        let mut span = build_span();
        processor.on_start(&mut span, &cx);

        let data = span.exported_data().expect("span should have data");
        let keys: Vec<&str> = data.attributes.iter().map(|kv| kv.key.as_str()).collect();

        assert!(keys.contains(&"user.id"));
        assert!(keys.contains(&"tenant.id"));
        assert!(!keys.contains(&"secret"));
    }

    #[test]
    fn no_op_when_baggage_is_empty() {
        let processor = BaggageSpanProcessor::new([Key::from_static_str("user.id")]);
        let cx = Context::new();

        let mut span = build_span();
        processor.on_start(&mut span, &cx);

        let data = span.exported_data().expect("span should have data");
        assert!(data.attributes.is_empty());
    }

    #[test]
    fn no_op_when_allowlist_is_empty() {
        let processor = BaggageSpanProcessor::new([]);
        let cx = Context::new().with_baggage([KeyValue::new("user.id", "42")]);

        let mut span = build_span();
        processor.on_start(&mut span, &cx);

        let data = span.exported_data().expect("span should have data");
        assert!(data.attributes.is_empty());
    }
}
