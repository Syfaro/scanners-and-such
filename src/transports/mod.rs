use serde::Serialize;

#[cfg(feature = "hid")]
pub mod hid;

pub mod usb;

#[cfg(any(feature = "hid", feature = "usb"))]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsbFilter {
    pub vendor_id: u16,
    pub product_id: u16,
}
