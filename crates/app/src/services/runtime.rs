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

/// Aborts the wrapped tokio task if dropped before it finishes. Lets a gpui
/// task's cancellation (e.g. a view dropped on navigation) propagate down and
/// stop the underlying IO instead of leaking it on the runtime.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Spawn `fut` on the IO runtime; await the returned future from gpui.
///
/// If the returned future is dropped before completion (the awaiting gpui task
/// was cancelled), the spawned IO task is aborted rather than left running.
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
        let mut guard = AbortOnDrop(handle);
        match std::pin::Pin::new(&mut guard.0).await {
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
