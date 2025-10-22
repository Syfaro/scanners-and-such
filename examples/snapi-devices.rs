use futures::StreamExt;
use scanners_and_such::{
    scanner::snapi,
    transports::hid::{HidTransport, OpenableHidDevice, UsbFilter},
};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt()
        .pretty()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    smol::block_on(async {
        let mut devices =
            scanners_and_such::transports::hid::HidDefaultTransport::get_devices(&[UsbFilter {
                vendor_id: snapi::USB_VENDOR_ID,
                product_id: snapi::USB_PRODUCT_ID,
            }])
            .await
            .unwrap();

        info!("{devices:#?}");

        let Some(device) = devices.pop() else {
            warn!("no devices found");
            return;
        };

        let (device, packets) = device
            .open::<{ snapi::packet::PACKET_LEN }>()
            .await
            .unwrap();
        let (mut device, mut packets) = snapi::Snapi::new(device, packets).await.unwrap();

        let serial_number = device.get_attribute(534).await.unwrap();
        info!(?serial_number);

        while let Some(event) = packets.next().await {
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
