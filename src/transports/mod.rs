#[cfg(feature = "hid")]
pub mod hid;

#[cfg(feature = "usb")]
pub mod usb;

#[cfg(any(feature = "hid", feature = "usb"))]
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsbFilter {
    pub vendor_id: u16,
    pub product_id: u16,
}
