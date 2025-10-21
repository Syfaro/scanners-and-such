use futures::StreamExt;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt()
        .pretty()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    smol::block_on(async {
        let mut snapi_devices = scanners_and_such::scanner::snapi::Snapi::discover_devices(true)
            .await
            .unwrap();

        info!("{snapi_devices:#?}");

        let Some(device) = snapi_devices.pop() else {
            warn!("no devices found");
            return;
        };

        let (mut device, mut events) = device.open().await.unwrap();

        while let Some(event) = events.next().await {
            info!("got event: {}", hex::encode(&event));

            match device.process_packet(event).await {
                Ok(Some(output)) => {
                    info!("got output: {output:?}");
                }
                Ok(None) => {
                    debug!("packet was not complete output");
                }
                Err(err) => {
                    error!("error processing packet: {err}");
                }
            }
        }
    });
}
