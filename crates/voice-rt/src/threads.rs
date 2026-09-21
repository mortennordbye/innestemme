use std::io;
use std::thread::{Builder, JoinHandle};

/// Spawns a named thread, pinned to `core` when given.
///
/// Pinning is best effort: macOS has no hard affinity API and a pod without an exclusive cpuset
/// may not own the requested core. Failure is reported through the return value of the closure's
/// first argument rather than treated as fatal.
pub fn spawn_pinned<F, T>(name: &str, core: Option<usize>, f: F) -> io::Result<JoinHandle<T>>
where
    F: FnOnce(bool) -> T + Send + 'static,
    T: Send + 'static,
{
    Builder::new().name(name.to_owned()).spawn(move || {
        let pinned = core.is_some_and(|id| core_affinity::set_for_current(core_affinity::CoreId { id }));
        f(pinned)
    })
}

/// Sizes the global rayon pool that candle runs its kernels on.
///
/// rayon defaults to the host's core count, not the cgroup's cpuset, so an unconfigured pool
/// oversubscribes a pod that owns fewer cores than the node has. Must run before the first
/// candle op. Returns false if the pool was already built.
pub fn configure_compute_threads(threads: usize) -> bool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads.max(1))
        .thread_name(|i| format!("compute-{i}"))
        .build_global()
        .is_ok()
}
