pub fn export_trace<B>(
    ctx: &opentelemetry::Context,
    m: &mut fe2o3_amqp_types::messaging::message::Message<B>,
) {
    opentelemetry::global::get_text_map_propagator(|p| {
        let annotations = m
            .message_annotations
            .get_or_insert_with(|| Default::default());

        p.inject_context(ctx, &mut AnnotationsInjector(annotations))
    });
}

pub fn import_trace<B>(
    m: &fe2o3_amqp_types::messaging::message::Message<B>,
    span: &mut impl opentelemetry::trace::Span,
) {
    use opentelemetry::trace::TraceContextExt;
    opentelemetry::global::get_text_map_propagator(|p| {
        if let Some(annotations) = &m.message_annotations {
            let context = p.extract(&AnnotationsExtractor(annotations));
            span.add_link(context.span().span_context().clone(), Vec::new());
        }
    });
}

struct AnnotationsInjector<'a>(&'a mut fe2o3_amqp_types::messaging::annotations::Annotations);

impl opentelemetry::propagation::Injector for AnnotationsInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        let key = fe2o3_amqp_types::primitives::Symbol::new(key);
        let key = fe2o3_amqp_types::messaging::annotations::OwnedKey::from(key);

        self.0.insert(key, value.into());
    }
}

struct AnnotationsExtractor<'a>(&'a fe2o3_amqp_types::messaging::annotations::Annotations);

impl opentelemetry::propagation::Extractor for AnnotationsExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        let key = fe2o3_amqp_types::primitives::Symbol::new(key);
        let key = fe2o3_amqp_types::messaging::annotations::OwnedKey::from(key);
        self.0.get(&key).and_then(|a| match a {
            fe2o3_amqp_types::primitives::Value::String(ref s) => Some(s as &str),
            _ => None,
        })
    }

    fn keys(&self) -> Vec<&str> {
        self.0
            .keys()
            .filter_map(|a| match a {
                fe2o3_amqp_types::messaging::annotations::OwnedKey::Symbol(s) => Some(s.as_str()),
                fe2o3_amqp_types::messaging::annotations::OwnedKey::Ulong(_) => None,
            })
            .collect()
    }
}
