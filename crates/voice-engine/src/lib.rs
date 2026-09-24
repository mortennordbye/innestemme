//! UDP audio server: reorder, decode, hand 80 ms frames to a pinned model thread, send the result back.

pub mod assistant;
pub mod engine;
pub mod metrics;
pub mod pipeline;
pub mod satellite;
pub mod settings;
pub mod transport;

pub use engine::{Engine, FrameProcessor, LoopbackEngine, MimiProcessor, Passthrough, ProcessError, StepOutcome};
pub use metrics::Metrics;
pub use pipeline::{serve, Config};
pub use transport::{Transport, UdpTransport};
