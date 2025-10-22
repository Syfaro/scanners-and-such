use async_trait::async_trait;
use futures::{SinkExt, Stream, channel::mpsc};
use tracing::{error, trace};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    Event, HidDeviceRequestOptions, HidInputReportEvent,
    js_sys::{Array, Reflect, Uint8Array},
    wasm_bindgen::{JsCast, JsValue, prelude::Closure},
};

use crate::transports::hid::{
    ClosableHidDevice, HidDiscoveredDevice, HidError, HidTransport, OpenableHidDevice, UsbFilter,
    WritableHidDevice,
};

impl HidError {
    fn new(message: impl Into<String>, inner: Option<JsValue>) -> Self {
        Self {
            message: message.into(),
            inner,
        }
    }
}

pub struct HidTransportWeb;

pub struct HidDevice {
    device: web_sys::HidDevice,
    // Hold the reference to the closure until we drop the device.
    listener: Closure<dyn FnMut(Event)>,
}

impl Drop for HidDevice {
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

#[async_trait(?Send)]
impl HidTransport for HidTransportWeb {
    async fn get_devices(filters: &[UsbFilter]) -> Result<Vec<HidDiscoveredDevice>, HidError> {
        let filters = serde_wasm_bindgen::to_value(filters)
            .map_err(|err| HidError::new("failed to serialize filters", Some(err.into())))?;
        let request_opts = HidDeviceRequestOptions::new(&filters);

        let window =
            web_sys::window().ok_or_else(|| HidError::new("missing window object", None))?;
        let navigator = window.navigator();

        if !Reflect::has(&navigator, &JsValue::from_str("hid"))
            .map_err(|err| HidError::new("could not check navigator for hid", Some(err)))?
        {
            return Err(HidError::new("navigator was missing hid object", None));
        }

        let hid = navigator.hid();

        let devices = JsFuture::from(hid.request_device(&request_opts))
            .await
            .map_err(|err| HidError::new("request device call failed", Some(err)))?;

        let devices: Vec<_> = devices
            .dyn_into::<Array>()
            .map_err(|err| HidError::new("hid devices were not array", Some(err)))?
            .into_iter()
            .map(|device| {
                device
                    .dyn_into::<web_sys::HidDevice>()
                    .map(|device| HidDiscoveredDevice { device })
            })
            .collect::<Result<_, _>>()
            .map_err(|err| HidError::new("returned object was not hid device", Some(err)))?;

        Ok(devices)
    }
}

impl HidDiscoveredDevice {
    fn process_event<const BUFFER_LEN: usize>(
        tx: mpsc::Sender<Vec<u8>>,
        ev: Event,
    ) -> Result<(), HidError> {
        let input_evt: HidInputReportEvent = ev
            .dyn_into()
            .map_err(|_| HidError::new("event was not hid input report", None))?;

        let arr = Uint8Array::new(&input_evt.data().buffer());

        let report_id = input_evt.report_id();
        let len = usize::try_from(arr.length()).expect("arr length should always fit in usize");
        trace!(len, "got hid input report: {report_id:02X}");

        if len - 1 > BUFFER_LEN {
            return Err(HidError::new(
                format!("packet length {len} was greater than buffer length {BUFFER_LEN}",),
                None,
            ));
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
impl OpenableHidDevice for HidDiscoveredDevice {
    type Device = HidDevice;

    async fn open<const BUFFER_LEN: usize>(
        self,
    ) -> Result<(Self::Device, impl Stream<Item = Vec<u8>> + Send), HidError> {
        JsFuture::from(self.device.open())
            .await
            .map_err(|err| HidError::new("failed to open device", Some(err)))?;

        let (tx, rx) = mpsc::channel(1);

        let listener = Closure::wrap(Box::new(move |ev: Event| {
            if let Err(err) = Self::process_event::<BUFFER_LEN>(tx.clone(), ev) {
                error!("could not process input report: {err}");
            }
        }) as Box<dyn FnMut(Event)>);

        self.device
            .add_event_listener_with_callback("inputreport", listener.as_ref().unchecked_ref())
            .map_err(|err| HidError::new("failed to add report callback", Some(err)))?;

        Ok((
            HidDevice {
                device: self.device,
                listener,
            },
            rx,
        ))
    }
}

#[async_trait(?Send)]
impl WritableHidDevice for HidDevice {
    async fn write_report(&mut self, data: &mut [u8]) -> Result<(), HidError> {
        let promise = self
            .device
            .send_report_with_u8_slice(data[0], &mut data[1..])
            .map_err(|err| HidError::new("failed to prepare sending report", Some(err)))?;

        JsFuture::from(promise)
            .await
            .map_err(|err| HidError::new("failed to write report", Some(err)))?;

        Ok(())
    }
}

#[async_trait(?Send)]
impl ClosableHidDevice for HidDevice {
    async fn close(&mut self) -> Result<(), HidError> {
        let promise = self.device.close();

        JsFuture::from(promise)
            .await
            .map_err(|err| HidError::new("failed to close device", Some(err)))?;

        Ok(())
    }
}
