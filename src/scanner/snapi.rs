use std::borrow::Cow;

use async_hid::{AsyncHidRead, AsyncHidWrite, DeviceReaderWriter};
use futures::{StreamExt, future};
use itertools::{Itertools, Position};
use tap::TapFallible;
use tracing::{Instrument, debug, instrument, trace, warn};

pub const SNAPI_VENDOR_ID: u16 = 0x05E0;
pub const SNAPI_PRODUCT_ID: u16 = 0x1900;

pub const SNAPI_PACKET_LEN: usize = 32;

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum SnapiInput {
    Status = 0x21,
    Attribute = 0x27,
}

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum SnapiOutput {
    Attribute = 0x0D,
}

#[derive(Debug, thiserror::Error)]
pub enum SnapiError {
    #[error("hid error: {0}")]
    Hid(#[from] async_hid::HidError),
    #[error("unexpected value {value:02X} when parsing field {name}")]
    UnexpectedValue { value: u8, name: Cow<'static, str> },
    #[error("got bad response code: {:?}", status.response_code)]
    BadStatus { status: SnapiStatus },
    #[error("attribute string was not valid utf-8")]
    InvalidString { data: Vec<u8> },
}

#[derive(Debug)]
pub struct SnapiDeviceInfo {
    pub name: String,
    pub serial_number: Option<String>,
}

pub struct Snapi {
    device: DeviceReaderWriter,
}

impl Snapi {
    #[instrument]
    pub async fn discover_devices(
        read_serial_numbers: bool,
    ) -> Result<Vec<SnapiDeviceInfo>, SnapiError> {
        let backend = async_hid::HidBackend::default();

        let devices = backend
            .enumerate()
            .await?
            .filter(|device| {
                future::ready(
                    device.vendor_id == SNAPI_VENDOR_ID && device.product_id == SNAPI_PRODUCT_ID,
                )
            })
            .filter_map(|device| {
                async move {
                    debug!(id = ?device.id, "found device");

                    if read_serial_numbers {
                        let name = device.name.clone();

                        let mut snapi_device = Self::new(device)
                            .await
                            .tap_err(|err| warn!("could not open snapi device: {err}"))
                            .ok()?;

                        let serial_number = snapi_device
                            .get_attribute(534)
                            .await
                            .tap_err(|err| warn!("could not get serial number: {err}"))
                            .ok()?;

                        match serial_number {
                            Some(SnapiAttributeValue::String(serial_number)) => {
                                Some(SnapiDeviceInfo {
                                    name,
                                    serial_number: Some(serial_number),
                                })
                            }
                            other => {
                                warn!("serial number was unexpected type: {other:?}");
                                None
                            }
                        }
                    } else {
                        Some(SnapiDeviceInfo {
                            name: device.name.clone(),
                            serial_number: None,
                        })
                    }
                }
                .in_current_span()
            })
            .collect()
            .await;

        Ok(devices)
    }

    pub async fn new(device: async_hid::Device) -> Result<Self, SnapiError> {
        let device = device.open().await?;

        let mut device = Self { device };
        device.initialize_device().await?;

        Ok(device)
    }

    /// There are a number of "magic" commands that need to be run to get
    /// everything working as expected.
    #[instrument(skip(self))]
    async fn initialize_device(&mut self) -> Result<(), SnapiError> {
        static COMMANDS: [[u8; 9]; 2] = [
            [0x00, 0x06, 0x20, 0x00, 0x04, 0xB0, 0x00, 0x00, 0x00],
            [0x00, 0x09, 0x05, 0x00, 0x4E, 0x2A, 0x42, 0x00, 0x01],
        ];

        debug!("initializing device");
        for command in COMMANDS {
            self.write_attribute_command(&command).await?;
        }

        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn get_attribute(
        &mut self,
        attribute_id: u16,
    ) -> Result<Option<SnapiAttributeValue>, SnapiError> {
        let mut command = Vec::with_capacity(6);
        command.extend_from_slice(&[0x00, 0x06, 0x02, 0x00]);
        command.extend_from_slice(&attribute_id.to_be_bytes());

        self.write_attribute_command(&command).await
    }

    #[instrument(skip_all)]
    async fn write_attribute_command(
        &mut self,
        command: &[u8],
    ) -> Result<Option<SnapiAttributeValue>, SnapiError> {
        for packet in snapi_encode_attribute_request(command) {
            trace!("writing command: {}", hex::encode(&packet));
            self.device.write_output_report(&packet).await?;
            self.read_status().await?.error_for_status()?;
        }

        self.read_attribute().await
    }

    #[instrument(skip(self, input))]
    async fn write_ack(&mut self, input: SnapiInput) -> Result<(), SnapiError> {
        let packet = [0x01, input as u8, 0x01];
        trace!(?input, "writing ack: {}", hex::encode(packet));
        self.device.write_output_report(&packet).await?;

        Ok(())
    }

    #[instrument(skip(self))]
    async fn read_status(&mut self) -> Result<SnapiStatus, SnapiError> {
        trace!("reading status");
        let mut buf = [0u8; SNAPI_PACKET_LEN];
        self.device.read_input_report(&mut buf).await?;

        match buf[0] {
            0x21 => Ok(SnapiStatus {
                report_id: buf[1],
                response_code: buf[2].try_into()?,
                extended_response_code: buf[3].try_into()?,
            }),
            value => Err(SnapiError::UnexpectedValue {
                value,
                name: "status".into(),
            }),
        }
    }

    #[instrument(skip(self))]
    async fn read_attribute(&mut self) -> Result<Option<SnapiAttributeValue>, SnapiError> {
        let mut buf = [0u8; SNAPI_PACKET_LEN];
        let mut output = Vec::new();

        loop {
            trace!("reading attribute response");
            self.device.read_input_report(&mut buf).await?;
            self.write_ack(SnapiInput::Attribute).await?;

            match buf[0] {
                0x27 => {
                    trace!(
                        has_next = buf[1] & 0b00010000 > 0,
                        len = buf[2],
                        "got attribute packet: {}",
                        hex::encode(buf)
                    );

                    output.extend_from_slice(&buf[3..3 + usize::from(buf[2])]);

                    if buf[1] & 0b00010000 == 0 {
                        break;
                    }
                }
                value => {
                    return Err(SnapiError::UnexpectedValue {
                        value,
                        name: "attribute".into(),
                    });
                }
            }
        }

        trace!("got raw attribute data: {output:?}");

        if output.len() < 6 {
            return Ok(None);
        }

        let value = match output[6] {
            b'F' => SnapiAttributeValue::Flag(output[8] > 0),
            b'B' => SnapiAttributeValue::Byte(output[8]),
            b'W' => SnapiAttributeValue::Word(u16::from_le_bytes([output[8], output[9]])),
            b'D' => SnapiAttributeValue::DWord(u32::from_le_bytes([
                output[8], output[9], output[10], output[11],
            ])),
            b'S' => {
                // Strings are null terminated which we don't need, so ignore
                // the last byte.
                let data = &output[12..12 + usize::from(output[9]) - 1];
                trace!("attempting to parse string from {}", hex::encode(data));
                SnapiAttributeValue::String(String::from_utf8(data.to_vec()).map_err(|err| {
                    SnapiError::InvalidString {
                        data: err.into_bytes(),
                    }
                })?)
            }
            b'A' => todo!(),
            0 => return Ok(None),
            value => {
                return Err(SnapiError::UnexpectedValue {
                    value,
                    name: "attribute type".into(),
                });
            }
        };

        Ok(Some(value))
    }
}

#[derive(Debug)]
pub struct SnapiStatus {
    pub report_id: u8,
    pub response_code: SnapiResponseCode,
    pub extended_response_code: SnapiExtendedResponseCode,
}

impl SnapiStatus {
    pub fn error_for_status(self) -> Result<(), SnapiError> {
        trace!(?self, "checking status");
        match self.response_code {
            SnapiResponseCode::Success => Ok(()),
            _ => Err(SnapiError::BadStatus { status: self }),
        }
    }
}

#[derive(Debug)]
pub enum SnapiResponseCode {
    Success = 0x01,
    Fail = 0x02,
    NotSupported = 0x03,
    SupportedNotCompleted = 0x04,
}

impl TryFrom<u8> for SnapiResponseCode {
    type Error = SnapiError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        let response_code = match value {
            0x01 => SnapiResponseCode::Success,
            0x02 => SnapiResponseCode::Fail,
            0x03 => SnapiResponseCode::NotSupported,
            0x04 => SnapiResponseCode::SupportedNotCompleted,
            value => {
                return Err(SnapiError::UnexpectedValue {
                    value,
                    name: "response code".into(),
                });
            }
        };

        Ok(response_code)
    }
}

#[derive(Debug)]
pub enum SnapiExtendedResponseCode {
    Empty = 0x00,
    AllParametersStored = 0x01,
    NoParametersStored = 0x02,
    SomeParametersStored = 0x03,
}

impl TryFrom<u8> for SnapiExtendedResponseCode {
    type Error = SnapiError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        let response_code = match value {
            0x00 => SnapiExtendedResponseCode::Empty,
            0x01 => SnapiExtendedResponseCode::AllParametersStored,
            0x02 => SnapiExtendedResponseCode::NoParametersStored,
            0x03 => SnapiExtendedResponseCode::SomeParametersStored,
            value => {
                return Err(SnapiError::UnexpectedValue {
                    value,
                    name: "extended response code".into(),
                });
            }
        };

        Ok(response_code)
    }
}

#[derive(Debug)]
pub enum SnapiAttributeValue {
    Flag(bool),
    Byte(u8),
    Word(u16),
    DWord(u32),
    String(String),
    Array(Vec<u8>),
}

#[derive(Debug, PartialEq)]
pub struct SnapiAttributeRequestFlags {
    pub has_next: bool,
    pub start: bool,
}

impl From<u8> for SnapiAttributeRequestFlags {
    fn from(value: u8) -> Self {
        Self {
            has_next: value & 0b10000000 > 0,
            start: value & 0b01000000 > 0,
        }
    }
}

impl From<SnapiAttributeRequestFlags> for u8 {
    fn from(value: SnapiAttributeRequestFlags) -> Self {
        ((value.has_next as u8) << 7) | ((value.start as u8) << 6)
    }
}

#[derive(Debug, PartialEq)]
pub struct SnapiAttributeResponseFlags {
    pub has_next: bool,
}

impl From<u8> for SnapiAttributeResponseFlags {
    fn from(value: u8) -> Self {
        Self {
            has_next: value & 0b00010000 > 0,
        }
    }
}

impl From<SnapiAttributeResponseFlags> for u8 {
    fn from(value: SnapiAttributeResponseFlags) -> Self {
        (value.has_next as u8) << 5
    }
}

/// Encode a SNAPI attribute request into as many packets as are needed.
///
/// Data may not be longer than `u16::MAX` otherwise this function will panic.
#[instrument]
pub fn snapi_encode_attribute_request(data: &[u8]) -> impl Iterator<Item = Vec<u8>> {
    let total_length = u16::try_from(data.len()).unwrap().to_be_bytes();

    // Each packet can be up to the maximum length minus the 4 header bytes, so
    // we can chunk the data on that size. We also need to know the position of
    // the chunk we're looking at to ensure the start and has_next flags can be
    // correctly set.
    data.chunks(SNAPI_PACKET_LEN - 4)
        .with_position()
        .map(move |(pos, chunk)| {
            trace!(?pos, len = chunk.len(), "needs packet for data");

            let flags = SnapiAttributeRequestFlags {
                has_next: !matches!(pos, Position::Only | Position::Last),
                start: matches!(pos, Position::First | Position::Only),
            };
            trace!(?flags, "got packet flags");

            let mut packet = Vec::with_capacity(SNAPI_PACKET_LEN);
            packet.push(SnapiOutput::Attribute as u8);
            packet.push(flags.into());
            packet.extend_from_slice(&total_length);
            packet.extend_from_slice(chunk);

            if packet.len() < SNAPI_PACKET_LEN {
                packet.extend(std::iter::repeat_n(0x00, SNAPI_PACKET_LEN - packet.len()));
            }

            debug_assert!(
                packet.len() <= SNAPI_PACKET_LEN,
                "snapi packet must be at most {SNAPI_PACKET_LEN} bytes long, was {} bytes",
                packet.len()
            );

            packet
        })
}

#[cfg(test)]
mod tests {
    use tracing_test::traced_test;

    use super::*;

    #[test]
    fn test_snapi_attribute_request_flags() {
        assert_eq!(
            SnapiAttributeRequestFlags::from(0b11000000),
            SnapiAttributeRequestFlags {
                has_next: true,
                start: true
            }
        );

        assert_eq!(
            SnapiAttributeRequestFlags::from(0b00000000),
            SnapiAttributeRequestFlags {
                has_next: false,
                start: false
            }
        );

        assert_eq!(
            <SnapiAttributeRequestFlags as Into<u8>>::into(SnapiAttributeRequestFlags {
                has_next: true,
                start: true
            }),
            0b11000000 as u8
        );

        assert_eq!(
            <SnapiAttributeRequestFlags as Into<u8>>::into(SnapiAttributeRequestFlags {
                has_next: false,
                start: false
            }),
            0b00000000 as u8
        );
    }

    #[traced_test]
    #[test]
    fn test_snapi_encode_attribute_request() {
        let packets: Vec<_> =
            snapi_encode_attribute_request(&[0x00, 0x06, 0x01, 0x00, 0x00, 0x00]).collect();

        assert_eq!(
            packets,
            vec![vec![
                0x0D, 0x40, 0x00, 0x06, 0x00, 0x06, 0x01, 0x00, 0x00, 0x00
            ]]
        );
    }
}
