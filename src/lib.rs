pub mod scanner;

pub(crate) mod runtime {
    #[cfg(feature = "runtime-smol")]
    pub(crate) fn spawn(fut: impl Future<Output = ()> + Send + 'static) {
        smol::spawn(async move {
            fut.await;
        })
        .detach();
    }

    #[cfg(feature = "runtime-tokio")]
    pub(crate) fn spawn(fut: impl Future<Output = ()> + Send + 'static) {
        tokio::spawn(async move {
            fut.await;
        });
    }
}
