use async_trait::async_trait;
use futures::{SinkExt, Stream, channel::mpsc};
use tracing::{error, trace};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    Event, HidDeviceFilter, HidDeviceRequestOptions, HidInputReportEvent,
    js_sys::{Reflect, Uint8Array},
    wasm_bindgen::{JsCast, JsValue, prelude::Closure},
};

use crate::transports::hid::{HidDevice, HidTransport, UsbFilter};

#[derive(Debug)]
pub struct WebHidError {
    pub message: String,
}

impl WebHidError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for WebHidError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for WebHidError {}

pub struct HidTransportWeb;

pub struct HidDeviceWeb {
    device: web_sys::HidDevice,
    // Hold the reference to the closure until we drop the device.
    listener: Closure<dyn FnMut(Event)>,
}

impl Drop for HidDeviceWeb {
    fn drop(&mut self) {
        if let Err(err) = self.device.remove_event_listener_with_callback(
            "inputreport",
            self.listener.as_ref().unchecked_ref(),
        ) {
            error!("could not remove event listener: {err:?}");
        }

        let device = self.device.clone();

        crate::runtime::spawn(async move {
            if let Err(err) = JsFuture::from(device.close()).await {
                error!("could not close hid device: {err:?}");
            }
        });
    }
}

impl HidTransportWeb {
    fn get_hid() -> Result<web_sys::Hid, WebHidError> {
        let window = web_sys::window().ok_or_else(|| WebHidError::new("missing window object"))?;
        let navigator = window.navigator();

        if !Reflect::has(&navigator, &JsValue::from_str("hid"))
            .map_err(|_err| WebHidError::new("could not check navigator for hid"))?
        {
            return Err(WebHidError::new("navigator was missing hid object"));
        }

        Ok(navigator.hid())
    }
}

#[async_trait(?Send)]
impl HidTransport for HidTransportWeb {
    type DiscoveredDevice = web_sys::HidDevice;
    type Error = WebHidError;

    async fn get_devices(filters: &[UsbFilter]) -> Result<Vec<web_sys::HidDevice>, WebHidError> {
        let filters: Vec<_> = filters
            .iter()
            .map(|filter| {
                let hid_filter = HidDeviceFilter::new();
                hid_filter.set_vendor_id(filter.vendor_id.into());
                hid_filter.set_product_id(filter.product_id);
                hid_filter
            })
            .collect();

        let request_opts = HidDeviceRequestOptions::new(&filters);

        let hid = Self::get_hid()?;

        let devices = JsFuture::from(hid.request_device(&request_opts))
            .await
            .map_err(|_err| WebHidError::new("request device call failed"))?;

        Ok(devices.to_vec())
    }
}

impl HidDeviceWeb {
    fn process_event<const BUFFER_LEN: usize>(
        tx: mpsc::Sender<Vec<u8>>,
        ev: Event,
    ) -> Result<(), WebHidError> {
        let input_evt: HidInputReportEvent = ev
            .dyn_into()
            .map_err(|_| WebHidError::new("event was not hid input report"))?;

        let arr = Uint8Array::new(&input_evt.data().buffer());

        let report_id = input_evt.report_id();
        let len = usize::try_from(arr.length()).expect("arr length should always fit in usize");
        trace!(len, "got hid input report: {report_id:02X}");

        if len - 1 > BUFFER_LEN {
            return Err(WebHidError::new(format!(
                "packet length {len} was greater than buffer length {BUFFER_LEN}"
            )));
        }

        let mut buf = vec![0u8; len + 1];
        buf[0] = report_id;
        arr.copy_to(&mut buf[1..len + 1]);

        let mut tx = tx.clone();

        crate::runtime::spawn(async move {
            if tx.send(buf).await.is_err() {
                error!("could not send input report");
            }
        });

        Ok(())
    }
}

#[async_trait(?Send)]
impl HidDevice for HidDeviceWeb {
    type DiscoveredDevice = web_sys::HidDevice;
    type Error = WebHidError;

    async fn new<const BUFFER_LEN: usize>(
        discovered_device: web_sys::HidDevice,
    ) -> Result<(Self, impl Stream<Item = Vec<u8>>), WebHidError> {
        JsFuture::from(discovered_device.open())
            .await
            .map_err(|_err| WebHidError::new("failed to open device"))?;

        let (tx, rx) = mpsc::channel(1);

        let listener = Closure::wrap(Box::new(move |ev: Event| {
            if let Err(err) = Self::process_event::<BUFFER_LEN>(tx.clone(), ev) {
                error!("could not process input report: {}", err.message);
            }
        }) as Box<dyn FnMut(Event)>);

        discovered_device
            .add_event_listener_with_callback("inputreport", listener.as_ref().unchecked_ref())
            .map_err(|_err| WebHidError::new("failed to add report callback"))?;

        Ok((
            HidDeviceWeb {
                device: discovered_device,
                listener,
            },
            rx,
        ))
    }

    async fn write_report(&mut self, data: &mut [u8]) -> Result<(), WebHidError> {
        let promise = self
            .device
            .send_report_with_u8_slice(data[0], &mut data[1..])
            .map_err(|_err| WebHidError::new("failed to prepare sending report"))?;

        JsFuture::from(promise)
            .await
            .map_err(|_err| WebHidError::new("failed to write report"))?;

        Ok(())
    }

    async fn close(&mut self) -> Result<(), WebHidError> {
        let promise = self.device.close();

        JsFuture::from(promise)
            .await
            .map_err(|_err| WebHidError::new("failed to close device"))?;

        Ok(())
    }
}
