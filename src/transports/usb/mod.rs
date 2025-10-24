use async_trait::async_trait;
use wasm_bindgen_futures::JsFuture;
use web_sys::wasm_bindgen::{JsCast, JsError, JsValue};
use web_sys::{
    UsbDeviceRequestOptions, UsbInTransferResult,
    js_sys::{Reflect, Uint8Array},
};

use crate::transports::UsbFilter;

#[async_trait(?Send)]
pub trait UsbTransport {
    type PlatformDevice;
    type Error;

    async fn get_devices(filters: &[UsbFilter]) -> Result<Vec<Self::PlatformDevice>, Self::Error>;
}

#[async_trait(?Send)]
pub trait UsbDevice: Sized {
    type PlatformDevice;
    type Error: std::fmt::Debug;

    type PlatformTransportBulkInput: UsbDeviceTransportInput<Error = Self::Error>;

    async fn new(platform_device: Self::PlatformDevice) -> Result<Self, Self::Error>;

    fn serial_number(&mut self) -> Option<String>;

    async fn select_configuration(&mut self, configuration: u8) -> Result<(), Self::Error>;

    async fn claim_interface(&mut self, interface: u8) -> Result<(), Self::Error>;
    async fn release_interface(&mut self, interface: u8) -> Result<(), Self::Error>;

    async fn claim_bulk_input_endpoint(
        &mut self,
        address: u8,
    ) -> Result<Self::PlatformTransportBulkInput, Self::Error>;

    async fn close(&mut self) -> Result<(), Self::Error>;
}

#[async_trait(?Send)]
pub trait UsbDeviceTransportInput {
    type Error;

    async fn transfer_in(&mut self, address: u8, buf: &mut [u8]) -> Result<usize, Self::Error>;
}

pub struct UsbTransportWeb;

impl UsbTransportWeb {
    fn get_usb() -> Result<web_sys::Usb, JsError> {
        let window = web_sys::window().ok_or_else(|| JsError::new("missing window object"))?;
        let navigator = window.navigator();

        if !Reflect::has(&navigator, &JsValue::from_str("usb"))
            .map_err(|_| JsError::new("could not check navigator for usb"))?
        {
            return Err(JsError::new("navigator was missing usb object"));
        }

        Ok(navigator.usb())
    }
}

#[async_trait(?Send)]
impl UsbTransport for UsbTransportWeb {
    type PlatformDevice = web_sys::UsbDevice;
    type Error = JsError;

    async fn get_devices(filters: &[UsbFilter]) -> Result<Vec<Self::PlatformDevice>, Self::Error> {
        let usb = Self::get_usb()?;

        let options = UsbDeviceRequestOptions::new(&serde_wasm_bindgen::to_value(&filters)?);

        let device = JsFuture::from(usb.request_device(&options))
            .await
            .map_err(|err| JsError::new(&format!("could not request devices: {err:?}")))?
            .dyn_into()
            .map_err(|_| JsError::new("could not convert device to USBDevice"))?;

        Ok(vec![device])
    }
}

#[derive(Clone)]
pub struct UsbDeviceWeb {
    device: web_sys::UsbDevice,
}

#[async_trait(?Send)]
impl UsbDevice for UsbDeviceWeb {
    type PlatformDevice = web_sys::UsbDevice;
    type PlatformTransportBulkInput = Self;

    type Error = JsError;

    async fn new(platform_device: Self::PlatformDevice) -> Result<Self, Self::Error> {
        JsFuture::from(platform_device.open())
            .await
            .map_err(|_| JsError::new("could not open device"))?;

        Ok(UsbDeviceWeb {
            device: platform_device,
        })
    }

    fn serial_number(&mut self) -> Option<String> {
        self.device.serial_number()
    }

    async fn select_configuration(&mut self, configuration: u8) -> Result<(), Self::Error> {
        JsFuture::from(self.device.select_configuration(configuration))
            .await
            .map_err(|err| JsError::new(&format!("could not select configuration: {err:?}")))?;

        Ok(())
    }

    async fn claim_interface(&mut self, interface: u8) -> Result<(), Self::Error> {
        JsFuture::from(self.device.claim_interface(interface))
            .await
            .map_err(|err| JsError::new(&format!("could not select interface: {err:?}")))?;

        Ok(())
    }

    async fn release_interface(&mut self, interface: u8) -> Result<(), Self::Error> {
        JsFuture::from(self.device.release_interface(interface))
            .await
            .map_err(|err| JsError::new(&format!("could not release interface: {err:?}")))?;

        Ok(())
    }

    async fn claim_bulk_input_endpoint(&mut self, _address: u8) -> Result<Self, Self::Error> {
        Ok(self.clone())
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        JsFuture::from(self.device.close())
            .await
            .map_err(|err| JsError::new(&format!("could not close interface: {err:?}")))?;

        Ok(())
    }
}

#[async_trait(?Send)]
impl UsbDeviceTransportInput for UsbDeviceWeb {
    type Error = JsError;

    async fn transfer_in(&mut self, address: u8, buf: &mut [u8]) -> Result<usize, Self::Error> {
        let transfer_result: UsbInTransferResult = JsFuture::from(
            self.device.transfer_in(
                address,
                buf.len()
                    .try_into()
                    .expect("buf len should always fit in u32"),
            ),
        )
        .await
        .map_err(|err| JsError::new(&format!("could not transfer in: {err:?}")))?
        .dyn_into()
        .map_err(|_| JsError::new("result was not USBInTransferResult"))?;

        let Some(data) = transfer_result.data() else {
            return Ok(0);
        };

        let arr = Uint8Array::new(&data.buffer());
        let len = arr
            .byte_length()
            .try_into()
            .expect("arr len should always fit in usize");
        arr.copy_to(&mut buf[0..len]);

        Ok(len)
    }
}
