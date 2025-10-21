use async_hid::{AsyncHidRead, AsyncHidWrite};
use async_trait::async_trait;
use futures::{Stream, StreamExt, TryFutureExt, future};
use tracing::error;

use crate::transports::hid::{
    HidDiscoveredDevice, HidError, HidTransport, OpenableHidDevice, UsbFilter, WritableHidDevice,
};

impl HidError {
    fn new(message: impl Into<String>, inner: Option<async_hid::HidError>) -> Self {
        Self {
            message: message.into(),
            inner,
        }
    }
}

pub struct HidTransportNative;

pub struct HidDevice {
    wtr: async_hid::DeviceWriter,
}

#[async_trait(?Send)]
impl HidTransport for HidTransportNative {
    async fn get_devices(
        filters: &[UsbFilter],
    ) -> Result<Vec<HidDiscoveredDevice>, HidError> {
        let backend = async_hid::HidBackend::default();
        let devices = backend
            .enumerate()
            .await
            .map_err(|err| HidError::new("could not enumerate devices", Some(err)))?
            .filter(|device| {
                future::ready(filters.iter().any(|filter| {
                    device.vendor_id == filter.vendor_id && device.product_id == filter.product_id
                }))
            })
            .map(|device| HidDiscoveredDevice { device })
            .collect()
            .await;

        Ok(devices)
    }
}

#[async_trait(?Send)]
impl OpenableHidDevice for HidDiscoveredDevice {
    type Device = HidDevice;

    async fn open<const BUFFER_LEN: usize>(
        self,
    ) -> Result<(Self::Device, impl Stream<Item = Vec<u8>>), HidError> {
        let (rdr, wtr) = self
            .device
            .open()
            .map_err(|err| HidError::new("could not open device", Some(err)))
            .await?;

        let device = HidDevice { wtr };

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
}

#[async_trait]
impl WritableHidDevice for HidDevice {
    async fn write_report(&mut self, data: &mut [u8]) -> Result<(), HidError> {
        self.wtr
            .write_output_report(data)
            .await
            .map_err(|err| HidError::new("could not write output report", Some(err)))
    }
}
