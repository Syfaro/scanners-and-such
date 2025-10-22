use std::sync::{Arc, atomic::AtomicBool};

use futures::{StreamExt, lock::Mutex};
use scanners_and_such::{
    scanner::snapi,
    transports::hid::{HidTransport, OpenableHidDevice, UsbFilter},
};
use tracing::{debug, error};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use tracing_web::{MakeWebConsoleWriter, performance_layer};
use wasm_bindgen::{JsError, JsValue, prelude::wasm_bindgen};
use wasm_bindgen_futures::js_sys;

#[wasm_bindgen(js_name = "enableLogging")]
pub fn enable_logging() {
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

type SnapiDevice = snapi::Snapi<scanners_and_such::transports::hid::HidDefaultDevice>;

#[wasm_bindgen]
pub struct SnapiDeviceManager {
    device: Arc<Mutex<Option<SnapiDevice>>>,
    is_running: Arc<AtomicBool>,
}

#[wasm_bindgen]
impl SnapiDeviceManager {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self {
            device: Arc::new(Mutex::new(None)),
            is_running: Default::default(),
        }
    }

    #[wasm_bindgen]
    pub async fn connected(&self) -> bool {
        self.device.lock().await.is_some()
    }

    #[wasm_bindgen]
    pub async fn start(&self, f: js_sys::Function) -> Result<(), JsError> {
        let mut devices =
            scanners_and_such::transports::hid::HidDefaultTransport::get_devices(&[UsbFilter {
                vendor_id: snapi::USB_VENDOR_ID,
                product_id: snapi::USB_PRODUCT_ID,
            }])
            .await?;

        let Some(device) = devices.pop() else {
            return Err(JsError::new("no devices found"));
        };

        let (device, packets) = device.open::<{ snapi::packet::PACKET_LEN }>().await?;
        let (device, mut packets) = snapi::Snapi::new(device, packets).await?;

        *self.device.lock().await = Some(device);
        self.is_running
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let is_running = Arc::clone(&self.is_running);
        let device = Arc::clone(&self.device);

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

                let value = serde_wasm_bindgen::to_value(&value).unwrap();

                if !f.is_function() {
                    error!("f isn't valid function");
                    break;
                }

                if let Err(err) = f.call1(&JsValue::null(), &value) {
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

        let device = self.device.lock().await.take();
        if let Some(device) = device {
            device.close().await?;
        }

        Ok(())
    }

    #[wasm_bindgen(js_name = "setAim")]
    pub async fn set_aim(&self, enabled: bool) -> Result<(), JsError> {
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
    pub async fn set_scanner(&self, enabled: bool) -> Result<(), JsError> {
        self.device
            .lock()
            .await
            .as_mut()
            .ok_or_else(|| JsError::new("device not yet started"))?
            .set_scanner(enabled)
            .await?;

        Ok(())
    }

    #[wasm_bindgen(js_name = "getAttribute")]
    pub async fn get_attribute(&self, attribute_id: u16) -> Result<JsValue, JsError> {
        let value = self
            .device
            .lock()
            .await
            .as_mut()
            .ok_or_else(|| JsError::new("device not yet started"))?
            .get_attribute(attribute_id)
            .await?;

        let value = serde_wasm_bindgen::to_value(&value)?;

        Ok(value)
    }
}
