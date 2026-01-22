use async_hid::{AsyncHidRead, AsyncHidWrite};
use async_trait::async_trait;
use futures::{Stream, StreamExt, future};
use tracing::error;

use crate::transports::hid::{HidDevice, HidTransport, UsbFilter};

pub struct HidTransportNative;

#[async_trait(?Send)]
impl HidTransport for HidTransportNative {
    type DiscoveredDevice = async_hid::Device;
    type Error = async_hid::HidError;

    async fn get_devices(
        filters: &[UsbFilter],
    ) -> Result<Vec<Self::DiscoveredDevice>, Self::Error> {
        let backend = async_hid::HidBackend::default();
        let devices = backend
            .enumerate()
            .await?
            .filter(|device| {
                future::ready(filters.iter().any(|filter| {
                    device.vendor_id == filter.vendor_id && device.product_id == filter.product_id
                }))
            })
            .collect()
            .await;

        Ok(devices)
    }
}

pub struct NativeHidDevice {
    wtr: async_hid::DeviceWriter,
}

#[async_trait]
impl HidDevice for NativeHidDevice {
    type DiscoveredDevice = async_hid::Device;
    type Error = async_hid::HidError;

    async fn new<const BUFFER_LEN: usize>(
        discovered_device: Self::DiscoveredDevice,
    ) -> Result<(Self, impl Stream<Item = Vec<u8>>), async_hid::HidError> {
        let (rdr, wtr) = discovered_device.open().await?;

        let device = NativeHidDevice { wtr };

        let stream =
            futures::stream::unfold((rdr, [0u8; BUFFER_LEN]), |(mut rdr, mut buf)| async move {
                let data = match rdr.read_input_report(&mut buf).await {
                    Ok(len) => buf[..len].to_vec(),
                    Err(err) => {
                        error!("reading input report failed: {err}");
                        return None;
                    }
                };

                Some((data, (rdr, buf)))
            });

        Ok((device, stream))
    }

    async fn write_report(&mut self, data: &mut [u8]) -> Result<(), Self::Error> {
        self.wtr.write_output_report(data).await
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        // We don't have exclusive use over HID devices on native platforms, so
        // nothing needs to happen here.
        Ok(())
    }
}
