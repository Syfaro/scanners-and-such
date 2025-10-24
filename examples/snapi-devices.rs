use futures::StreamExt;
use scanners_and_such::{
    scanner::snapi,
    transports::{
        UsbFilter,
        hid::{HidTransport, OpenableHidDevice},
        usb::{UsbDevice, UsbTransport},
    },
};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

enum EventType {
    Packet(Vec<u8>),
    Data(Result<scanners_and_such::scanner::snapi::SnapiData, snapi::SnapiError>),
}

fn main() {
    tracing_subscriber::fmt()
        .pretty()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let filters = [UsbFilter {
        vendor_id: snapi::USB_VENDOR_ID,
        product_id: snapi::USB_PRODUCT_ID,
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

        let mut usb_devices =
            scanners_and_such::transports::usb::native::UsbTransportNative::get_devices(&filters)
                .await
                .unwrap();

        info!("{usb_devices:#?}");

        let Some(usb_device) = usb_devices.pop() else {
            warn!("no usb devices found");
            return;
        };

        let usb_device =
            scanners_and_such::transports::usb::native::UsbDeviceNative::new(usb_device)
                .await
                .unwrap();

        let (device, packets) = hid_device
            .open::<{ snapi::packet::PACKET_LEN }>()
            .await
            .unwrap();
        let (mut device, packets) = snapi::Snapi::new(device, packets).await.unwrap();
        let packets = packets.map(|packet| EventType::Packet(packet));

        let data = device.attach_usb_device(usb_device).await.unwrap();
        let data = data.map(|data| EventType::Data(data));

        let serial_number = device.get_attribute(534).await.unwrap();
        info!(?serial_number);

        let attribute_ids = device.get_all_attribute_ids().await.unwrap();
        info!("device has {} attributes", attribute_ids.len());

        device.set_mode(snapi::SnapiMode::Image).await.unwrap();

        let mut events = futures::stream::select(packets, data);

        while let Some(event) = events.next().await {
            match event {
                EventType::Packet(packet) => match device.process_packet(packet).await {
                    Ok(Some(output)) => {
                        info!("got output: {output:?}");
                    }
                    Ok(None) => {
                        debug!("packet was not complete output");
                    }
                    Err(err) => {
                        error!("error processing packet: {err}");
                    }
                },
                EventType::Data(Ok(data)) => {
                    info!(mode = ?data.mode, "got {} bytes of data", data.body.len());
                }
                EventType::Data(Err(err)) => {
                    panic!("got error from data channel: {err}");
                }
            }
        }
    });
}
