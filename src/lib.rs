#[cfg(feature = "amqp")]
pub mod amqp;

#[cfg(feature = "init")]
mod baggage_span_processor;
#[cfg(feature = "init")]
pub use baggage_span_processor::BaggageSpanProcessor;

#[cfg(feature = "grpc")]
pub mod grpc;

#[cfg(feature = "init")]
mod init;
#[cfg(feature = "init")]
pub use init::{init, init_with_baggage, Handle};
