//! Global tokio runtime bridging IO/audio work to gpui's executor.
//!
//! reqwest and stream-download need a tokio reactor; gpui has its own
//! executor. We spawn IO futures on this runtime and await the resulting
//! `JoinHandle` (a runtime-agnostic Future) from gpui tasks.

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::Runtime;

fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("io-runtime")
            .build()
            .expect("failed to build tokio runtime")
    })
}

/// Spawn `fut` on the IO runtime; await the returned future from gpui.
///
/// Panics in the spawned future surface as an `Err` here (JoinError is
/// converted to a readable message rather than propagating the panic).
pub fn spawn_io<T, F>(fut: F) -> impl Future<Output = anyhow::Result<T>> + Send
where
    T: Send + 'static,
    F: Future<Output = anyhow::Result<T>> + Send + 'static,
{
    let handle = runtime().spawn(fut);
    async move {
        match handle.await {
            Ok(result) => result,
            Err(e) => Err(anyhow::anyhow!("io task failed: {e}")),
        }
    }
}

/// Run a closure inside the runtime context (for constructing types that
/// require an active tokio reactor, e.g. the playback engine).
pub fn enter<T>(f: impl FnOnce() -> T) -> T {
    let _guard = runtime().enter();
    f()
}
