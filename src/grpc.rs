#[derive(Clone, Copy)]
pub struct TraceInjector;

impl tonic::service::Interceptor for TraceInjector {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        opentelemetry::global::get_text_map_propagator(|p| {
            p.inject(&mut MetadataInjector(req.metadata_mut()))
        });
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
    tower_layer::layer_fn(TraceExtractor)
}

#[derive(Clone)]
pub struct TraceExtractor<Svc>(Svc);

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
        self.0.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        use opentelemetry::trace::FutureExt;

        let context = opentelemetry::global::get_text_map_propagator(|p| {
            p.extract(&HeadersExtractor(req.headers()))
        });

        self.0.call(req).with_context(context)
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
