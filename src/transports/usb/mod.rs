use async_trait::async_trait;

use crate::transports::UsbFilter;

#[cfg(not(target_arch = "wasm32"))]
pub mod native;
// #[cfg(any(target_arch = "wasm32", feature = "web"))]
#[cfg(target_arch = "wasm32")]
pub mod web;

#[async_trait(?Send)]
pub trait UsbTransport {
    type DiscoveredDevice;
    type Error;

    async fn get_devices(filters: &[UsbFilter])
    -> Result<Vec<Self::DiscoveredDevice>, Self::Error>;
}

#[async_trait(?Send)]
pub trait UsbDevice: Sized {
    type DiscoveredDevice;
    type Error: std::fmt::Debug;

    #[cfg(target_arch = "wasm32")]
    type PlatformTransportBulkInput: UsbDeviceTransportInput<Error = Self::Error>;
    #[cfg(not(target_arch = "wasm32"))]
    type PlatformTransportBulkInput: UsbDeviceTransportInput<Error = Self::Error> + Send;

    async fn new(discovered_device: Self::DiscoveredDevice) -> Result<Self, Self::Error>;

    fn serial_number(&mut self) -> Option<String>;

    async fn select_configuration(&mut self, configuration: u8) -> Result<(), Self::Error>;

    async fn claim_interface(&mut self, interface: u8) -> Result<(), Self::Error>;
    async fn release_interface(&mut self, interface: u8) -> Result<(), Self::Error>;

    async fn claim_bulk_input_endpoint(
        &mut self,
        interface: u8,
        address: u8,
        buffer_size: usize,
    ) -> Result<Self::PlatformTransportBulkInput, Self::Error>;

    async fn close(self) -> Result<(), Self::Error>;
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait UsbDeviceTransportInput {
    type Error;

    async fn transfer_in(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error>;
}
