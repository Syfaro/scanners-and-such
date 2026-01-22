use async_trait::async_trait;
use futures::Stream;

use crate::transports::UsbFilter;

#[cfg(not(target_arch = "wasm32"))]
pub mod native;
#[cfg(target_arch = "wasm32")]
pub mod web;

#[async_trait(?Send)]
pub trait HidTransport {
    type DiscoveredDevice;
    type Error;

    async fn get_devices(filters: &[UsbFilter])
    -> Result<Vec<Self::DiscoveredDevice>, Self::Error>;
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait HidDevice: Sized {
    type DiscoveredDevice;
    type Error: std::fmt::Debug;

    async fn new<const PACKET_LEN: usize>(
        discovered_device: Self::DiscoveredDevice,
    ) -> Result<(Self, impl Stream<Item = Vec<u8>>), Self::Error>;

    async fn write_report(&mut self, data: &mut [u8]) -> Result<(), Self::Error>;

    async fn close(&mut self) -> Result<(), Self::Error>;
}
