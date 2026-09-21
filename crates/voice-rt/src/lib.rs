//! Real-time plumbing shared by the engine and the test client: reorder buffer, allocation guards,
//! pinned threads and lock-free histograms.

pub mod hist;
pub mod jitter;
pub mod noalloc;
pub mod threads;
