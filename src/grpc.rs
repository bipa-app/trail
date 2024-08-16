pub fn trace_injector() -> TraceInjector {
    TraceInjector(opentelemetry_contrib::trace::propagator::trace_context_response::TraceContextResponsePropagator::new())
}

#[derive(Clone)]
pub struct TraceInjector(opentelemetry_contrib::trace::propagator::trace_context_response::TraceContextResponsePropagator);

impl tonic::service::Interceptor for TraceInjector {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        use opentelemetry::propagation::text_map_propagator::TextMapPropagator;
        self.0.inject(&mut MetadataInjector(req.metadata_mut()));
        Ok(req)
    }
}

struct MetadataInjector<'a>(&'a mut tonic::metadata::MetadataMap);

impl opentelemetry::propagation::Injector for MetadataInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        let key = key.parse::<tonic::metadata::AsciiMetadataKey>();
        let value = value.parse::<tonic::metadata::AsciiMetadataValue>();

        if let (Ok(key), Ok(value)) = (key, value) {
            self.0.insert(key, value);
        }
    }
}

pub fn trace_extractor<Svc>() -> impl Clone + tower_layer::Layer<Svc, Service = TraceExtractor<Svc>>
{
    let propagator = opentelemetry_contrib::trace::propagator::trace_context_response::TraceContextResponsePropagator::new();

    tower_layer::layer_fn(move |svc| TraceExtractor {
        propagator: propagator.clone(),
        svc,
    })
}

#[derive(Clone)]
pub struct TraceExtractor<Svc> {
    propagator: opentelemetry_contrib::trace::propagator::trace_context_response::TraceContextResponsePropagator,
    svc: Svc,
}

impl<B, S> tower_service::Service<http::Request<B>> for TraceExtractor<S>
where
    S: tower_service::Service<http::Request<B>>,
{
    type Error = S::Error;
    type Response = S::Response;
    type Future = opentelemetry::trace::WithContext<S::Future>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.svc.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        use opentelemetry::propagation::text_map_propagator::TextMapPropagator;
        use opentelemetry::trace::FutureExt;

        let context = self.propagator.extract(&HeadersExtractor(req.headers()));
        self.svc.call(req).with_context(context)
    }
}

struct HeadersExtractor<'a>(&'a http::HeaderMap);

impl opentelemetry::propagation::Extractor for HeadersExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|a| a.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(http::HeaderName::as_str).collect()
    }
}
