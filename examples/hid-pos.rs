use scanners_and_such::{
    scanner::hid_pos::{self, HidPos},
    transports::{
        UsbFilter,
        hid::{HidDevice, HidTransport, native::NativeHidDevice},
    },
};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt()
        .pretty()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let filters = [UsbFilter {
        vendor_id: 0x0C2E,
        product_id: 0x1007,
    }];

    smol::block_on(async {
        let mut hid_devices =
            scanners_and_such::transports::hid::native::HidTransportNative::get_devices(&filters)
                .await
                .unwrap();

        info!("{hid_devices:#?}");

        let Some(hid_device) = hid_devices.pop() else {
            warn!("no hid devices found");
            return;
        };

        let (device, packets) = NativeHidDevice::new::<{ hid_pos::PACKET_LEN }>(hid_device)
            .await
            .unwrap();
        let (device, mut rx) = HidPos::new(device, packets).await;

        info!("opened device");

        while let Ok(packet) = rx.recv().await {
            info!("{packet:?}");
            println!("{}", String::from_utf8_lossy(&packet.value));
        }

        device.close().await.unwrap();
    });
}
