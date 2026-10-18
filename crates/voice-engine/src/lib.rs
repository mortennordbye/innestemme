//! UDP audio server: reorder, decode, hand 80 ms frames to a pinned model thread, send the result back.

pub mod activity;
pub mod assistant;
pub mod control;
pub mod engine;
pub mod hub;
pub mod metrics;
pub mod party;
pub mod pipeline;
pub mod satellite;
pub mod screen;
pub mod settings;
pub mod transport;
pub mod unhandled;
pub mod web;

pub use engine::{Engine, FrameProcessor, LoopbackEngine, MimiProcessor, Passthrough, ProcessError, StepOutcome};
pub use metrics::Metrics;
pub use pipeline::{serve, Config};
pub use transport::{Transport, UdpTransport};
