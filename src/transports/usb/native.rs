use std::collections::HashMap;

use async_trait::async_trait;
use nusb::transfer::Bulk;

use crate::transports::{
    UsbFilter,
    usb::{UsbDevice, UsbDeviceTransportInput, UsbTransport},
};

#[derive(Debug, thiserror::Error)]
pub enum UsbError {
    #[error("platform usb error: {0}")]
    Platform(#[from] nusb::Error),
    #[error("interface {interface} was not yet claimed")]
    UnclaimedInterface { interface: u8 },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub struct UsbTransportNative;

#[async_trait(?Send)]
impl UsbTransport for UsbTransportNative {
    type DiscoveredDevice = nusb::DeviceInfo;
    type Error = UsbError;

    async fn get_devices(
        filters: &[UsbFilter],
    ) -> Result<Vec<Self::DiscoveredDevice>, Self::Error> {
        nusb::list_devices()
            .await
            .map(|devices| {
                devices
                    .filter(|device| {
                        filters.iter().any(|filter| {
                            filter.vendor_id == device.vendor_id()
                                && filter.product_id == device.product_id()
                        })
                    })
                    .collect()
            })
            .map_err(Into::into)
    }
}

pub struct UsbDeviceNative {
    device: nusb::Device,
    serial_number: Option<String>,

    interfaces: HashMap<u8, nusb::Interface>,
}

#[async_trait(?Send)]
impl UsbDevice for UsbDeviceNative {
    type DiscoveredDevice = nusb::DeviceInfo;
    type Error = UsbError;

    type PlatformTransportBulkInput = nusb::io::EndpointRead<Bulk>;

    async fn new(discovered_device: Self::DiscoveredDevice) -> Result<Self, Self::Error> {
        discovered_device
            .open()
            .await
            .map(|device| Self {
                device,
                serial_number: discovered_device.serial_number().map(|s| s.to_string()),

                interfaces: HashMap::new(),
            })
            .map_err(Into::into)
    }

    fn serial_number(&mut self) -> Option<String> {
        self.serial_number.clone()
    }

    async fn select_configuration(&mut self, configuration: u8) -> Result<(), Self::Error> {
        self.device
            .set_configuration(configuration)
            .await
            .map_err(Into::into)
    }

    async fn claim_interface(&mut self, interface: u8) -> Result<(), Self::Error> {
        if self.interfaces.contains_key(&interface) {
            return Ok(());
        }

        let claimed_interface = self.device.claim_interface(interface).await?;

        self.interfaces.insert(interface, claimed_interface);

        Ok(())
    }

    async fn release_interface(&mut self, interface: u8) -> Result<(), Self::Error> {
        self.interfaces.remove(&interface);

        Ok(())
    }

    async fn claim_bulk_input_endpoint(
        &mut self,
        interface: u8,
        address: u8,
        buffer_size: usize,
    ) -> Result<Self::PlatformTransportBulkInput, Self::Error> {
        let Some(claimed_interface) = self.interfaces.get_mut(&interface) else {
            return Err(UsbError::UnclaimedInterface { interface });
        };

        let reader = claimed_interface.endpoint(address)?.reader(buffer_size);

        Ok(reader)
    }

    async fn close(self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[cfg(feature = "runtime-smol")]
#[async_trait]
impl UsbDeviceTransportInput for nusb::io::EndpointRead<Bulk> {
    type Error = UsbError;

    async fn transfer_in(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        use smol::io::AsyncReadExt;

        self.read(buf).await.map_err(Into::into)
    }
}

#[cfg(feature = "runtime-tokio")]
#[async_trait]
impl UsbDeviceTransportInput for nusb::io::EndpointRead<Bulk> {
    type Error = UsbError;

    async fn transfer_in(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        use tokio::io::AsyncReadExt;

        self.read(buf).await.map_err(Into::into)
    }
}
