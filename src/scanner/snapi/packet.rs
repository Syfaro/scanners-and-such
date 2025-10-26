use itertools::{Itertools, Position};
use num_enum::{FromPrimitive, IntoPrimitive, TryFromPrimitive};
use serde::{Deserialize, Serialize};
use tracing::{instrument, trace, warn};

use crate::scanner::snapi::{
    HidInput, HidOutput, SnapiError, SnapiNotification, code_types::CodeType,
};

pub const PACKET_LEN: usize = 32;

#[derive(Debug)]
pub enum SnapiPacket {
    Status(SnapiStatus),
    Barcode(SnapiBarcodePacket),
    Notification(SnapiNotification),
    Attribute(SnapiAttributePacket),
    Other { hid_input: HidInput, data: Vec<u8> },
}

impl SnapiPacket {
    pub fn decode_any(data: &[u8]) -> Result<Self, SnapiError> {
        let packet = match HidInput::from(data[0]) {
            HidInput::Status => SnapiStatus::decode(data)?.packet(),
            HidInput::Barcode => SnapiBarcodePacket::decode(data)?.packet(),
            HidInput::Notification => SnapiNotification::decode(data)?.packet(),
            HidInput::Attribute => SnapiAttributePacket::decode(data)?.packet(),
            hid_input => SnapiPacket::Other {
                hid_input,
                data: data.to_vec(),
            },
        };

        Ok(packet)
    }

    pub fn decode_specific<T: DecodableSnapiPacket>(data: &[u8]) -> Result<T, SnapiError> {
        let hid_input = HidInput::from(data[0]);

        if hid_input != T::HID_INPUT {
            return Err(SnapiError::UnexpectedValue {
                value: data[0],
                name: "decode".into(),
            });
        }

        T::decode(data)
    }
}

pub trait DecodableSnapiPacket: Sized {
    const HID_INPUT: HidInput;

    fn decode(data: &[u8]) -> Result<Self, SnapiError>;

    fn packet(self) -> SnapiPacket;
}

pub trait CollatableSnapiPacket: Sized {
    type Output: SnapiPacketOutput;

    fn collate(self, packets: &mut Vec<Self>) -> Result<Self::Output, SnapiError>;
}

pub trait SnapiPacketOutput: Sized {
    fn output(self) -> Option<SnapiOutput>;
}

impl<T: SnapiPacketOutput> SnapiPacketOutput for Option<T> {
    fn output(self) -> Option<SnapiOutput> {
        match self {
            Some(output) => output.output(),
            None => None,
        }
    }
}

macro_rules! require_length {
    ($data:expr, $min:expr) => {
        let len = $data.len();
        if len < $min {
            return Err(SnapiError::TooShort {
                expected: $min,
                actual: len,
            });
        }
    };
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapiStatus {
    pub hid_output: HidOutput,
    pub response_code: SnapiResponseCode,
    pub extended_response_code: SnapiExtendedResponseCode,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum SnapiResponseCode {
    Success = 0x01,
    Fail = 0x02,
    NotSupported = 0x03,
    SupportedNotCompleted = 0x04,
}

impl serde::Serialize for SnapiResponseCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u8((*self).into())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum SnapiExtendedResponseCode {
    Empty = 0x00,
    AllParametersStored = 0x01,
    NoParametersStored = 0x02,
    SomeParametersStored = 0x03,
}

impl serde::Serialize for SnapiExtendedResponseCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u8((*self).into())
    }
}

impl SnapiStatus {
    pub fn error_for_status(self, expected_hid_output: HidOutput) -> Result<(), SnapiError> {
        trace!(
            hid_output = ?self.hid_output,
            response_code = ?self.response_code,
            ?expected_hid_output,
            "checking status"
        );

        match self.response_code {
            SnapiResponseCode::Success if self.hid_output == expected_hid_output => Ok(()),
            _ => Err(SnapiError::BadStatus { status: self }),
        }
    }
}

impl DecodableSnapiPacket for SnapiStatus {
    const HID_INPUT: HidInput = HidInput::Status;

    fn decode(data: &[u8]) -> Result<Self, SnapiError> {
        require_length!(data, 4);

        Ok(SnapiStatus {
            hid_output: HidOutput::from(data[1]),
            response_code: data[2]
                .try_into()
                .map_err(|_| SnapiError::UnexpectedValue {
                    value: data[2],
                    name: "response code".into(),
                })?,
            extended_response_code: data[3].try_into().map_err(|_| {
                SnapiError::UnexpectedValue {
                    value: data[2],
                    name: "extended response code".into(),
                }
            })?,
        })
    }

    fn packet(self) -> SnapiPacket {
        SnapiPacket::Status(self)
    }
}

impl CollatableSnapiPacket for SnapiStatus {
    type Output = SnapiStatus;

    fn collate(self, _packets: &mut Vec<Self>) -> Result<Self::Output, SnapiError> {
        Ok(self)
    }
}

impl SnapiPacketOutput for SnapiStatus {
    fn output(self) -> Option<SnapiOutput> {
        Some(SnapiOutput::Status(self))
    }
}

#[derive(Debug)]
pub struct SnapiBarcodePacket {
    pub packet_count: usize,
    pub packet_index: usize,

    pub code_type: CodeType,
    pub data: Vec<u8>,
}

impl DecodableSnapiPacket for SnapiBarcodePacket {
    const HID_INPUT: HidInput = HidInput::Barcode;

    fn decode(data: &[u8]) -> Result<Self, SnapiError> {
        require_length!(data, 6);
        let packet_count = usize::from(data[1]);
        let packet_index = usize::from(data[2]);
        let length = usize::from(data[3]);
        let code_type = CodeType::from(u16::from_le_bytes([data[4], data[5]]));

        require_length!(data, 6 + length);
        let data = data[6..(6 + length)].to_vec();

        Ok(Self {
            packet_count,
            packet_index,
            code_type,
            data,
        })
    }

    fn packet(self) -> SnapiPacket {
        SnapiPacket::Barcode(self)
    }
}

impl CollatableSnapiPacket for SnapiBarcodePacket {
    type Output = Option<SnapiBarcode>;

    fn collate(self, packets: &mut Vec<Self>) -> Result<Self::Output, SnapiError> {
        if self.packet_count == 1 {
            trace!("got single packet barcode");

            if !packets.is_empty() {
                packets.clear();
                return Err(SnapiError::BadPacketOrder {
                    name: format!("expected 0 previous packets, had {}", packets.len()).into(),
                });
            }

            Ok(Some(SnapiBarcode {
                data: self.data,
                code_type: self.code_type,
            }))
        } else if self.packet_count == self.packet_index + 1 {
            trace!(count = self.packet_count, "got last packet in barcode");

            let expected_count = self.packet_count;
            let code_type = self.code_type;

            packets.push(self);

            if packets.len() != expected_count {
                packets.clear();
                return Err(SnapiError::BadPacketOrder {
                    name: format!("expected {expected_count} packets, had {}", packets.len())
                        .into(),
                });
            }

            let packets = std::mem::take(packets);

            let data = packets.into_iter().flat_map(|packet| packet.data).collect();

            Ok(Some(SnapiBarcode { data, code_type }))
        } else {
            let packet_index = self.packet_index;
            trace!(
                packet_index,
                packet_count = self.packet_count,
                "got packet of barcode",
            );

            packets.push(self);

            if packets.len() != packet_index + 1 {
                packets.clear();
                return Err(SnapiError::BadPacketOrder {
                    name: format!("on packet index {packet_index} but had {}", packets.len())
                        .into(),
                });
            }

            Ok(None)
        }
    }
}

impl SnapiPacketOutput for SnapiBarcode {
    fn output(self) -> Option<SnapiOutput> {
        Some(SnapiOutput::Barcode(self))
    }
}

#[derive(Debug)]
pub struct SnapiAttributePacket {
    pub flags: SnapiAttributeResponseFlags,
    pub data: Vec<u8>,
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

impl DecodableSnapiPacket for SnapiAttributePacket {
    const HID_INPUT: HidInput = HidInput::Attribute;

    fn decode(data: &[u8]) -> Result<Self, SnapiError> {
        require_length!(data, 3);
        let has_next = data[1] & 0b00010000 > 0;
        let length = usize::from(data[2]);

        require_length!(data, 3 + length);
        let data = data[3..(3 + length)].to_vec();

        Ok(Self {
            flags: SnapiAttributeResponseFlags { has_next },
            data,
        })
    }

    fn packet(self) -> SnapiPacket {
        SnapiPacket::Attribute(self)
    }
}

impl CollatableSnapiPacket for SnapiAttributePacket {
    type Output = Option<SnapiAttributeResponse>;

    fn collate(self, packets: &mut Vec<Self>) -> Result<Self::Output, SnapiError> {
        let has_next = self.flags.has_next;
        trace!(has_next, "processing attribute packet");

        packets.push(self);

        if has_next {
            return Ok(None);
        }

        let data: Vec<_> = std::mem::take(packets)
            .into_iter()
            .flat_map(|attribute| attribute.data)
            .collect();
        trace!("got packet data: {}", hex::encode(&data));
        let value = SnapiAttributeResponse::decode(&data)?;
        trace!(?value, "got attribute value");

        Ok(Some(value))
    }
}

impl SnapiPacketOutput for SnapiAttributeResponse {
    fn output(self) -> Option<SnapiOutput> {
        Some(SnapiOutput::Attribute(self))
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase", tag = "type", content = "value")]
pub enum SnapiOutput {
    Status(SnapiStatus),
    Barcode(SnapiBarcode),
    Notification(SnapiNotification),
    Attribute(SnapiAttributeResponse),
    Other { hid_input: HidInput, data: Vec<u8> },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapiBarcode {
    pub data: Vec<u8>,
    pub code_type: CodeType,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, FromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum SnapiAttributeRequest {
    GetId = 0x01,
    Get = 0x02,
    // GetNext = 0x03,
    GetOffset = 0x04,
    Set = 0x05,
    Store = 0x06,
    #[num_enum(catch_all)]
    Unknown(u8),
}

impl serde::Serialize for SnapiAttributeRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u8((*self).into())
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase", tag = "type", content = "value")]
pub enum SnapiAttributeResponse {
    GetId(Vec<u16>),
    Get(Option<SnapiAttributeValue>),
    GetOffset(Vec<u8>),
    Set,
    Store,
    Unknown {
        request: SnapiAttributeRequest,
        data: Vec<u8>,
    },
}

impl SnapiAttributeResponse {
    pub fn decode(data: &[u8]) -> Result<Self, SnapiError> {
        match SnapiAttributeRequest::from(data[2]) {
            SnapiAttributeRequest::GetId => {
                let ids = data[6..]
                    .chunks_exact(2)
                    .map(|chunk| {
                        u16::from_be_bytes(
                            chunk.try_into().expect("chunk must always be two bytes"),
                        )
                    })
                    .collect();
                Ok(SnapiAttributeResponse::GetId(ids))
            }
            SnapiAttributeRequest::Get => {
                SnapiAttributeValue::decode(data).map(SnapiAttributeResponse::Get)
            }
            SnapiAttributeRequest::GetOffset => {
                require_length!(data, 13);
                Ok(SnapiAttributeResponse::GetOffset(data[13..].to_vec()))
            }
            SnapiAttributeRequest::Set => Ok(SnapiAttributeResponse::Set),
            SnapiAttributeRequest::Store => Ok(SnapiAttributeResponse::Store),
            request @ SnapiAttributeRequest::Unknown(_) => Ok(SnapiAttributeResponse::Unknown {
                request,
                data: data.to_vec(),
            }),
        }
    }
}

#[cfg_attr(feature = "web", derive(tsify::Tsify))]
#[cfg_attr(feature = "web", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(
    Clone, Copy, PartialEq, Eq, Debug, FromPrimitive, IntoPrimitive, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum SnapiAttributeType {
    Flag = b'F',
    Byte = b'B',
    Word = b'W',
    DWord = b'D',
    String = b'S',
    Array = b'A',
    Action = b'X',
    Empty = 0x00,
    #[num_enum(catch_all)]
    Unknown(u8),
}

#[cfg_attr(feature = "web", derive(tsify::Tsify))]
#[cfg_attr(feature = "web", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "type", content = "value")]
pub enum SnapiAttributeValue {
    Flag(bool),
    Byte(u8),
    Word(u16),
    DWord(u32),
    String(String),
    Array(Vec<u8>),
    Action(u16),
    Unknown {
        attribute_type: SnapiAttributeType,
        data: Vec<u8>,
    },
}

impl SnapiAttributeValue {
    pub fn attribute_type(&self) -> SnapiAttributeType {
        match self {
            Self::Flag(_) => SnapiAttributeType::Flag,
            Self::Byte(_) => SnapiAttributeType::Byte,
            Self::Word(_) => SnapiAttributeType::Word,
            Self::DWord(_) => SnapiAttributeType::DWord,
            Self::String(_) => SnapiAttributeType::String,
            Self::Array(_) => SnapiAttributeType::Array,
            Self::Action(_) => SnapiAttributeType::Action,
            Self::Unknown { attribute_type, .. } => *attribute_type,
        }
    }

    pub fn decode(data: &[u8]) -> Result<Option<Self>, SnapiError> {
        if data.len() < 7 {
            return Ok(None);
        }

        let value = match SnapiAttributeType::from(data[6]) {
            SnapiAttributeType::Flag => {
                require_length!(data, 8);
                SnapiAttributeValue::Flag(data[8] > 0)
            }
            SnapiAttributeType::Byte => {
                require_length!(data, 8);
                SnapiAttributeValue::Byte(data[8])
            }
            SnapiAttributeType::Word => {
                require_length!(data, 9);
                SnapiAttributeValue::Word(u16::from_le_bytes([data[8], data[9]]))
            }
            SnapiAttributeType::DWord => {
                require_length!(data, 11);
                SnapiAttributeValue::DWord(u32::from_le_bytes([
                    data[8], data[9], data[10], data[11],
                ]))
            }
            SnapiAttributeType::String => {
                require_length!(data, 9);
                // Strings are null terminated, ignore the last byte.
                let pos = 12 + usize::from(data[9]) - 1;
                if pos <= 12 {
                    warn!("empty string");
                    SnapiAttributeValue::String(String::new())
                } else {
                    require_length!(data, pos);
                    let data = &data[12..pos];
                    trace!("attempting to parse string from {}", hex::encode(data));
                    String::from_utf8(data.to_vec())
                        .map(SnapiAttributeValue::String)
                        .unwrap_or_else(|err| {
                            let bytes = err.into_bytes();
                            warn!(
                                "expected string but not valid utf-8, returning as array: {}",
                                hex::encode(&bytes)
                            );
                            SnapiAttributeValue::Array(bytes)
                        })
                }
            }
            SnapiAttributeType::Array => {
                require_length!(data, 13);
                let len = usize::from(u16::from_be_bytes([data[9], data[10]]));
                let data = data[13..std::cmp::min(data.len(), len + 13)].to_vec();
                if data.len() < len {
                    return Err(SnapiError::PartialData {
                        expected: len,
                        got: data,
                    });
                } else {
                    SnapiAttributeValue::Array(data)
                }
            }
            SnapiAttributeType::Empty => return Ok(None),
            attribute_type => SnapiAttributeValue::Unknown {
                attribute_type,
                data: data.to_vec(),
            },
        };

        Ok(Some(value))
    }

    pub fn encode(&self) -> Option<Vec<u8>> {
        let data = match self {
            SnapiAttributeValue::Flag(value) => {
                vec![SnapiAttributeType::Flag.into(), 0x00, *value as u8]
            }
            SnapiAttributeValue::Byte(value) => vec![SnapiAttributeType::Byte.into(), 0x00, *value],
            SnapiAttributeValue::Word(value) => {
                let bytes = value.to_be_bytes();
                vec![SnapiAttributeType::Word.into(), bytes[0], bytes[1]]
            }
            SnapiAttributeValue::DWord(value) => {
                let bytes = value.to_be_bytes();
                vec![
                    SnapiAttributeType::DWord.into(),
                    bytes[0],
                    bytes[1],
                    bytes[2],
                    bytes[3],
                ]
            }
            SnapiAttributeValue::Action(value) => {
                let bytes = value.to_le_bytes();
                vec![SnapiAttributeType::Action.into(), bytes[0], bytes[1]]
            }
            SnapiAttributeValue::String(value) => {
                let mut bytes = Vec::with_capacity(6 + value.len());
                bytes.extend_from_slice(&[SnapiAttributeType::String.into(), 0x00]);
                bytes.extend_from_slice(&i16::try_from(value.len()).unwrap().to_be_bytes());
                bytes.extend_from_slice(&[0x00, 0x00]);
                bytes.extend_from_slice(value.as_bytes());
                bytes
            }
            SnapiAttributeValue::Array(_) => {
                todo!()
            }
            SnapiAttributeValue::Unknown { .. } => return None,
        };

        debug_assert!(
            SnapiAttributeType::from(data[0]) == self.attribute_type(),
            "encoded data must be of same type"
        );

        Some(data)
    }
}

/// Encode a SNAPI attribute request into as many packets as are needed.
///
/// Data may not be longer than `u16::MAX` otherwise this function will panic.
#[instrument]
pub(crate) fn encode_attribute_request(data: &[u8]) -> impl Iterator<Item = Vec<u8>> {
    let total_length = u16::try_from(data.len())
        .expect("data length must always fit in u16")
        .to_be_bytes();

    // Each packet can be up to the maximum length minus the 4 header bytes, so
    // we can chunk the data on that size. We also need to know the position of
    // the chunk we're looking at to ensure the start and has_next flags can be
    // correctly set.
    data.chunks(PACKET_LEN - 4)
        .with_position()
        .map(move |(pos, chunk)| {
            trace!(?pos, len = chunk.len(), "needs packet for data");

            let flags = SnapiAttributeRequestFlags {
                has_next: !matches!(pos, Position::Only | Position::Last),
                start: matches!(pos, Position::First | Position::Only),
            };
            trace!(?flags, "got packet flags");

            let mut packet = Vec::with_capacity(PACKET_LEN);
            packet.push(HidOutput::Attribute.into());
            packet.push(flags.into());
            packet.extend_from_slice(&total_length);
            packet.extend_from_slice(chunk);

            if packet.len() < PACKET_LEN {
                packet.extend(std::iter::repeat_n(0x00, PACKET_LEN - packet.len()));
            }

            debug_assert!(
                packet.len() <= PACKET_LEN,
                "snapi packet must be at most {PACKET_LEN} bytes long, was {} bytes",
                packet.len()
            );

            packet
        })
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
            encode_attribute_request(&[0x00, 0x06, 0x01, 0x00, 0x00, 0x00]).collect();

        assert_eq!(
            packets,
            vec![vec![
                0x0D, 0x40, 0x00, 0x06, 0x00, 0x06, 0x01, 0x00, 0x00, 0x00
            ]]
        );
    }
}
