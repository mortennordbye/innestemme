//! Allocation guards for the audio path.
//!
//! These only check anything in binaries that install `AllocDisabler` as the global allocator
//! (debug builds of the engine, and the loopback test). A violation aborts the process.
//! With the crate's default `disable_release` feature both calls compile to nothing in release.

#[cfg(debug_assertions)]
pub use assert_no_alloc::AllocDisabler;

/// Runs `f` and aborts if it allocates or frees.
#[inline]
pub fn no_alloc<T>(f: impl FnOnce() -> T) -> T {
    assert_no_alloc::assert_no_alloc(f)
}

/// Re-enables allocation inside a `no_alloc` section. Used around candle calls, which allocate a
/// tensor per op and cannot be made allocation-free from the outside.
#[inline]
pub fn permit_alloc<T>(f: impl FnOnce() -> T) -> T {
    assert_no_alloc::permit_alloc(f)
}
