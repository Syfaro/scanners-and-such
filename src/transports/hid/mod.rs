use async_trait::async_trait;
use futures::Stream;

use crate::transports::UsbFilter;

#[cfg(not(target_arch = "wasm32"))]
pub mod native;
#[cfg(target_arch = "wasm32")]
pub mod web;

#[derive(Debug)]
pub struct HidDiscoveredDevice {
    #[cfg(not(target_arch = "wasm32"))]
    device: async_hid::Device,
    #[cfg(target_arch = "wasm32")]
    device: web_sys::HidDevice,
}

#[derive(Debug, thiserror::Error)]
#[error("hid error: {message}")]
pub struct HidError {
    pub message: String,

    #[cfg(not(target_arch = "wasm32"))]
    pub inner: Option<async_hid::HidError>,
    #[cfg(target_arch = "wasm32")]
    pub inner: Option<web_sys::wasm_bindgen::JsValue>,
}

#[cfg(not(target_arch = "wasm32"))]
pub type HidDefaultTransport = native::HidTransportNative;
#[cfg(not(target_arch = "wasm32"))]
pub type HidDefaultDevice = native::HidDevice;

#[cfg(target_arch = "wasm32")]
pub type HidDefaultTransport = web::HidTransportWeb;
#[cfg(target_arch = "wasm32")]
pub type HidDefaultDevice = web::HidDevice;

#[async_trait(?Send)]
pub trait HidTransport {
    async fn get_devices(filters: &[UsbFilter]) -> Result<Vec<HidDiscoveredDevice>, HidError>;
}

#[async_trait(?Send)]
pub trait OpenableHidDevice {
    type Device: WritableHidDevice + ClosableHidDevice;

    async fn open<const PACKET_LEN: usize>(
        self,
    ) -> Result<(Self::Device, impl Stream<Item = Vec<u8>> + Send), HidError>;
}

#[async_trait(?Send)]
pub trait ClosableHidDevice {
    async fn close(&mut self) -> Result<(), HidError>;
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait WritableHidDevice {
    async fn write_report(&mut self, data: &mut [u8]) -> Result<(), HidError>;
}
