pub mod scanner;
pub mod transports;

pub(crate) mod runtime {
    #[cfg(all(feature = "runtime-smol", not(target_arch = "wasm32")))]
    pub(crate) fn spawn(fut: impl Future<Output = ()> + Send + 'static) {
        smol::spawn(async move {
            fut.await;
        })
        .detach();
    }

    #[cfg(all(feature = "runtime-tokio", not(target_arch = "wasm32")))]
    pub(crate) fn spawn(fut: impl Future<Output = ()> + Send + 'static) {
        tokio::spawn(async move {
            fut.await;
        });
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn spawn(fut: impl Future<Output = ()> + 'static) {
        wasm_bindgen_futures::spawn_local(fut);
    }
}
