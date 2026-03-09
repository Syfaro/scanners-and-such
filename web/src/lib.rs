#![cfg(target_arch = "wasm32")]

use std::{rc::Rc, sync::atomic::AtomicBool};

use futures::{StreamExt, lock::Mutex};
use scanners_and_such::{
    scanner::{hid_pos, snapi},
    transports::{hid::HidDevice, usb::UsbDevice},
};
use tracing::{debug, error};
use wasm_bindgen::{JsError, JsValue, prelude::wasm_bindgen};
use wasm_bindgen_futures::js_sys;
use web_sys::js_sys::Uint8Array;

#[cfg(feature = "logging")]
#[wasm_bindgen(js_name = "enableLogging")]
pub fn enable_logging() {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
    use tracing_web::{MakeWebConsoleWriter, performance_layer};

    console_error_panic_hook::set_once();

    let fmt_layer = tracing_subscriber::fmt::layer()
        .without_time()
        .with_writer(MakeWebConsoleWriter::new());
    let perf_layer = performance_layer()
        .with_details_from_fields(tracing_subscriber::fmt::format::Pretty::default());

    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(perf_layer)
        .init();
}

#[cfg(not(feature = "logging"))]
#[wasm_bindgen(js_name = "enableLogging")]
pub fn enable_logging() {}

type SnapiDevice = snapi::Snapi<
    scanners_and_such::transports::hid::web::HidDeviceWeb,
    scanners_and_such::transports::usb::web::UsbDeviceWeb,
>;

#[wasm_bindgen(js_name = "SnapiDevice")]
#[derive(Default)]
pub struct SnapiDeviceManager {
    device: Rc<Mutex<Option<SnapiDevice>>>,

    is_running: Rc<AtomicBool>,
    has_usb_device: Rc<AtomicBool>,
}

#[wasm_bindgen(js_class = "SnapiDevice")]
impl SnapiDeviceManager {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self::default()
    }

    #[wasm_bindgen(return_description = "if a device is currently connected")]
    pub async fn connected(&self) -> bool {
        self.device.lock().await.is_some()
    }

    #[wasm_bindgen(
        js_name = "usbDeviceAttached",
        return_description = "if a USB device is attached"
    )]
    pub async fn usb_device_attached(&self) -> bool {
        self.has_usb_device
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    #[wasm_bindgen(js_name = "start")]
    pub async fn start(
        &self,
        #[wasm_bindgen(param_description = "HID device to use")] device: web_sys::HidDevice,
        #[wasm_bindgen(param_description = "callback to be executed on every complete output")]
        callback: js_sys::Function,
    ) -> Result<(), JsError> {
        let (device, packets) = HidDevice::new::<{ snapi::packet::PACKET_LEN }>(device).await?;
        let (device, mut packets) = snapi::Snapi::new(device, packets).await?;

        *self.device.lock().await = Some(device);
        self.is_running
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let is_running = Rc::clone(&self.is_running);
        let device = Rc::clone(&self.device);

        wasm_bindgen_futures::spawn_local(async move {
            while let Some(packet) = packets.next().await {
                let mut device = device.lock().await;

                let Some(device) = device.as_mut() else {
                    error!("device disappeared after getting packet");
                    break;
                };

                let value = match device.process_packet(packet).await {
                    Ok(Some(value)) => value,
                    Ok(None) => continue,
                    Err(err) => {
                        error!("could not process packet: {err}");
                        break;
                    }
                };

                let value = serde_wasm_bindgen::to_value(&value)
                    .expect("it should always be possible to serialize SnapiOutput");

                if let Err(err) = callback.call1(&JsValue::null(), &value) {
                    error!("could not call provided function: {err:?}");
                }
            }

            if is_running.load(std::sync::atomic::Ordering::SeqCst) {
                error!("packet stream ended while device was running");
            } else {
                debug!("packet stream ended because of device shutdown");
            }
        });

        Ok(())
    }

    #[wasm_bindgen]
    pub async fn close(&self) -> Result<(), JsError> {
        self.is_running
            .store(false, std::sync::atomic::Ordering::SeqCst);

        if let Some(mut device) = self.device.lock().await.take() {
            if let Some(usb_device) = device.detach_usb_device().await? {
                self.has_usb_device
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                usb_device.close().await?;
            };

            device.close().await?;
        }

        Ok(())
    }

    #[wasm_bindgen(js_name = "attachUsbDevice")]
    pub async fn attach_usb_device(
        &self,
        device: web_sys::UsbDevice,
        callback: js_sys::Function,
    ) -> Result<(), JsError> {
        let device = scanners_and_such::transports::usb::web::UsbDeviceWeb::new(device)
            .await
            .map_err(|_| JsError::new("could not open device"))?;

        let mut rx = self
            .device
            .lock()
            .await
            .as_mut()
            .ok_or_else(|| JsError::new("device not yet started"))?
            .attach_usb_device(device)
            .await
            .map_err(|err| JsError::new(&format!("could not attach device: {err:?}")))?;

        self.has_usb_device
            .store(true, std::sync::atomic::Ordering::SeqCst);

        wasm_bindgen_futures::spawn_local(async move {
            while let Some(data) = rx.next().await {
                let (err, value) = match data {
                    Ok(data) => (
                        JsValue::null(),
                        Uint8Array::new_from_slice(&data.body).into(),
                    ),
                    Err(err) => (JsError::new(&err.to_string()).into(), JsValue::null()),
                };

                if let Err(err) = callback.call2(&JsValue::null(), &err, &value) {
                    error!("could not call provided function: {err:?}");
                }
            }
        });

        Ok(())
    }

    #[wasm_bindgen(js_name = "setMode")]
    pub async fn set_mode(&self, mode: snapi::SnapiMode) -> Result<(), JsError> {
        self.device
            .lock()
            .await
            .as_mut()
            .ok_or_else(|| JsError::new("device not yet started"))?
            .set_mode(mode)
            .await?;

        Ok(())
    }

    #[wasm_bindgen(js_name = "setAim")]
    pub async fn set_aim(
        &self,
        #[wasm_bindgen(param_description = "if the aiming dot shoult be enabled")] enabled: bool,
    ) -> Result<(), JsError> {
        self.device
            .lock()
            .await
            .as_mut()
            .ok_or_else(|| JsError::new("device not yet started"))?
            .set_aim(enabled)
            .await?;

        Ok(())
    }

    #[wasm_bindgen(js_name = "setScanner")]
    pub async fn set_scanner(
        &self,
        #[wasm_bindgen(param_description = "if the scanner shoult be enabled")] enabled: bool,
    ) -> Result<(), JsError> {
        self.device
            .lock()
            .await
            .as_mut()
            .ok_or_else(|| JsError::new("device not yet started"))?
            .set_scanner(enabled)
            .await?;

        Ok(())
    }

    #[wasm_bindgen(js_name = "setSoftTrigger")]
    pub async fn set_soft_trigger(
        &self,
        #[wasm_bindgen(param_description = "if the the trigger should be pulled")] enabled: bool,
    ) -> Result<(), JsError> {
        self.device
            .lock()
            .await
            .as_mut()
            .ok_or_else(|| JsError::new("device not yet started"))?
            .set_soft_trigger(enabled)
            .await?;

        Ok(())
    }

    #[wasm_bindgen(
        js_name = "getAttributeIds",
        return_description = "all known attribute IDs"
    )]
    pub async fn get_attribute_ids(&self) -> Result<Vec<u16>, JsError> {
        self.device
            .lock()
            .await
            .as_mut()
            .ok_or_else(|| JsError::new("device not yet started"))?
            .get_all_attribute_ids()
            .await
            .map_err(Into::into)
    }

    #[wasm_bindgen(
        js_name = "getAttribute",
        return_description = "the type and value of the attribute, if it exists"
    )]
    pub async fn get_attribute(
        &self,
        #[wasm_bindgen(js_name = "attributeId", param_description = "the attribute ID")]
        attribute_id: u16,
    ) -> Result<Option<snapi::packet::SnapiAttributeValue>, JsError> {
        let value = self
            .device
            .lock()
            .await
            .as_mut()
            .ok_or_else(|| JsError::new("device not yet started"))?
            .get_attribute(attribute_id)
            .await?;

        Ok(value)
    }

    #[wasm_bindgen(js_name = "setAttribute")]
    pub async fn set_attribute(
        &self,
        #[wasm_bindgen(js_name = "attributeId", param_description = "the attribute ID")]
        attribute_id: u16,
        #[wasm_bindgen(param_description = "if the value should be persisted")] store: bool,
        #[wasm_bindgen(param_description = "the type and value to store to the attribute")]
        value: snapi::packet::SnapiAttributeValue,
    ) -> Result<(), JsError> {
        self.device
            .lock()
            .await
            .as_mut()
            .ok_or_else(|| JsError::new("device not yet started"))?
            .set_attribute(attribute_id, store, value)
            .await?;

        Ok(())
    }
}

type HidPosDevice = hid_pos::HidPos<scanners_and_such::transports::hid::web::HidDeviceWeb>;

#[wasm_bindgen(js_name = "HidPosDevice")]
#[derive(Default)]
pub struct HidPosManager {
    device: Rc<Mutex<Option<HidPosDevice>>>,

    is_running: Rc<AtomicBool>,
}

#[wasm_bindgen(js_class = "HidPosDevice")]
impl HidPosManager {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self::default()
    }

    #[wasm_bindgen(js_name = "start")]
    pub async fn start(
        &self,
        #[wasm_bindgen(param_description = "HID device to use")] device: web_sys::HidDevice,
        #[wasm_bindgen(param_description = "callback to be executed on every complete output")]
        callback: js_sys::Function,
    ) -> Result<(), JsError> {
        let (device, packets) = HidDevice::new::<{ hid_pos::PACKET_LEN }>(device).await?;
        let (device, mut packets) = hid_pos::HidPos::new(device, packets).await;

        *self.device.lock().await = Some(device);
        self.is_running
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let is_running = Rc::clone(&self.is_running);

        wasm_bindgen_futures::spawn_local(async move {
            while let Some(packet) = packets.next().await {
                let value = serde_wasm_bindgen::to_value(&packet)
                    .expect("it should always be possible to serialize HidData");

                if let Err(err) = callback.call1(&JsValue::null(), &value) {
                    error!("could not call provided function: {err:?}");
                }
            }

            if is_running.load(std::sync::atomic::Ordering::SeqCst) {
                error!("packet stream ended while device was running");
            } else {
                debug!("packet stream ended because of device shutdown");
            }
        });

        Ok(())
    }

    #[wasm_bindgen]
    pub async fn close(&self) -> Result<(), JsError> {
        self.is_running
            .store(false, std::sync::atomic::Ordering::SeqCst);

        if let Some(device) = self.device.lock().await.take() {
            device.close().await?;
        }

        Ok(())
    }
}
