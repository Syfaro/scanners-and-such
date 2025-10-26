use std::{borrow::Cow, collections::HashMap, sync::Arc};

use futures::{
    SinkExt, Stream, StreamExt,
    channel::{mpsc, oneshot},
    future::Either,
    lock::Mutex,
};
use num_enum::{FromPrimitive, IntoPrimitive};
use serde::{Deserialize, Serialize};
use tracing::{Instrument, debug, error, info, instrument, trace, warn};

use crate::{
    scanner::snapi::packet::*,
    transports::{
        hid::{ClosableHidDevice, HidError, WritableHidDevice},
        usb::{UsbDevice, UsbDeviceTransportInput},
    },
};

pub mod code_types;
pub mod packet;

pub const USB_VENDOR_ID: u16 = 0x05E0;
pub const USB_PRODUCT_ID: u16 = 0x1900;

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, IntoPrimitive, FromPrimitive)]
#[repr(u8)]
pub enum HidInput {
    Status = 0x21,
    Barcode = 0x22,
    Notification = 0x24,
    Attribute = 0x27,
    #[num_enum(catch_all)]
    Unknown(u8),
}

impl serde::Serialize for HidInput {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u8((*self).into())
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, IntoPrimitive, FromPrimitive)]
#[repr(u8)]
pub enum HidOutput {
    Acknowledgement = 0x01,
    Aim = 0x02,
    Mode = 0x03,
    Scanner = 0x06,
    MacroPdf = 0x08,
    SoftTrigger = 0x0A,
    Attribute = 0x0D,
    #[num_enum(catch_all)]
    Unknown(u8),
}

impl serde::Serialize for HidOutput {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u8((*self).into())
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, IntoPrimitive, FromPrimitive)]
#[repr(u8)]
pub enum SnapiNotification {
    BarcodeMode = 0x10,
    ImageMode = 0x11,
    VideoMode = 0x12,
    #[num_enum(catch_all)]
    Unknown(u8),
}

impl DecodableSnapiPacket for SnapiNotification {
    const HID_INPUT: HidInput = HidInput::Notification;

    fn decode(data: &[u8]) -> Result<Self, SnapiError> {
        Ok(SnapiNotification::from(data[1]))
    }

    fn packet(self) -> SnapiPacket {
        SnapiPacket::Notification(self)
    }
}

impl SnapiPacketOutput for SnapiNotification {
    fn output(self) -> Option<SnapiOutput> {
        Some(SnapiOutput::Notification(self))
    }
}

impl serde::Serialize for SnapiNotification {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u8((*self).into())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SnapiError {
    #[error("hid error: {0}")]
    Hid(#[from] HidError),
    #[error("unexpected value {value:02X} when parsing field {name}")]
    UnexpectedValue { value: u8, name: Cow<'static, str> },
    #[error("got bad response code: {:?}", status.response_code)]
    BadStatus { status: SnapiStatus },
    #[error("encountered error when using channel: {name}")]
    Channel { name: &'static str },
    #[error("packets were missing or out of order: {name}")]
    BadPacketOrder { name: Cow<'static, str> },
    #[error("mismatched data type: {name}")]
    MismatchedData { name: &'static str },
    #[error("usb error: {message}")]
    Usb { message: String },
    #[error("data too short")]
    TooShort { expected: usize, actual: usize },
    #[error("only got partial data")]
    PartialData { expected: usize, got: Vec<u8> },
}

impl SnapiError {
    fn usb(err: impl std::fmt::Debug) -> Self {
        Self::Usb {
            message: format!("{err:?}"),
        }
    }
}

pub struct SnapiData {
    pub mode: SnapiMode,
    pub header: Vec<u8>,
    pub body: Vec<u8>,
}

/// A SNAPI device.
pub struct Snapi<H, U = ()> {
    hid_device: H,
    is_connected: bool,

    pending: Arc<Pending<Vec<u8>>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    reader_ended_rx: Option<oneshot::Receiver<()>>,

    barcode_packets: Vec<SnapiBarcodePacket>,
    attribute_packets: Vec<SnapiAttributePacket>,

    usb: Option<(U, mpsc::Sender<Result<SnapiData, SnapiError>>)>,
    cancel_usb_tasks: HashMap<u8, oneshot::Sender<oneshot::Sender<()>>>,
}

#[cfg_attr(feature = "web", derive(tsify::Tsify))]
#[cfg_attr(feature = "web", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, IntoPrimitive, FromPrimitive, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum SnapiMode {
    Barcode = 0x00,
    Image = 0x01,
    Video = 0x02,
    #[num_enum(catch_all)]
    Unknown(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoPrimitive)]
#[repr(u8)]
pub enum MacroPdfAction {
    Flush = 0x00,
    Abort = 0x01,
}

impl<H: WritableHidDevice, U> Snapi<H, U> {
    /// Create a new SNAPI device from a HID device and initialize it.
    pub async fn new(
        hid: H,
        packets: impl Stream<Item = Vec<u8>> + Send + 'static,
    ) -> Result<(Self, mpsc::Receiver<Vec<u8>>), SnapiError> {
        let (others_tx, others_rx) = mpsc::channel(8);
        let pending = Pending::new(others_tx);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (reader_ended_tx, reader_ended_rx) = oneshot::channel();

        let mut snapi_device = Self {
            hid_device: hid,
            is_connected: true,

            pending: Arc::clone(&pending),
            shutdown_tx: Some(shutdown_tx),
            reader_ended_rx: Some(reader_ended_rx),

            barcode_packets: Vec::new(),
            attribute_packets: Vec::new(),

            usb: None,
            cancel_usb_tasks: HashMap::new(),
        };

        // Spawn reader after creating device, because now device will send a
        // message to the shutdown channel when dropped.
        SnapiReader::start(packets, pending, shutdown_rx, reader_ended_tx);

        snapi_device.initialize_device().await?;

        Ok((snapi_device, others_rx))
    }

    /// Write an acknowledgement packet.
    ///
    /// Note that is only needed if you're not using the
    /// [`Snapi::process_packet`] function, as that handles this automatically.
    #[instrument(skip(self, input))]
    pub async fn write_ack(&mut self, input: HidInput) -> Result<(), SnapiError> {
        self.write_command(
            HidOutput::Acknowledgement,
            &mut [HidOutput::Acknowledgement.into(), input.into(), 0x01],
            false,
        )
        .await
    }

    #[instrument(skip(self))]
    pub async fn set_mode(&mut self, mode: SnapiMode) -> Result<(), SnapiError> {
        self.write_command(
            HidOutput::Mode,
            &mut [HidOutput::Mode.into(), mode.into()],
            true,
        )
        .await
    }

    /// Set if the aiming dot should be enabled.
    #[instrument(skip(self))]
    pub async fn set_aim(&mut self, enabled: bool) -> Result<(), SnapiError> {
        self.write_command(
            HidOutput::Aim,
            &mut [HidOutput::Aim.into(), enabled as u8],
            true,
        )
        .await
    }

    /// Set if the scanner should be enabled.
    #[instrument(skip(self))]
    pub async fn set_scanner(&mut self, enabled: bool) -> Result<(), SnapiError> {
        self.write_command(
            HidOutput::Scanner,
            &mut [HidOutput::Scanner.into(), enabled as u8],
            true,
        )
        .await
    }

    #[instrument(skip(self))]
    pub async fn send_macro_pdf_action(
        &mut self,
        action: MacroPdfAction,
    ) -> Result<(), SnapiError> {
        self.write_command(
            HidOutput::MacroPdf,
            &mut [HidOutput::MacroPdf.into(), action.into()],
            true,
        )
        .await
    }

    #[instrument(skip(self))]
    pub async fn set_soft_trigger(&mut self, enabled: bool) -> Result<(), SnapiError> {
        self.write_command(
            HidOutput::SoftTrigger,
            &mut [HidOutput::SoftTrigger.into(), enabled as u8],
            true,
        )
        .await
    }

    #[instrument(skip(self))]
    pub async fn get_attribute_ids(&mut self, offset: u16) -> Result<Vec<u16>, SnapiError> {
        let offset_bytes = offset.to_be_bytes();
        let command = [0x00, 0x06, 0x01, 0x00, offset_bytes[0], offset_bytes[1]];

        match self.write_attribute_command(&command).await? {
            SnapiAttributeResponse::GetId(ids) => Ok(ids),
            _ => Err(SnapiError::MismatchedData {
                name: "attribute ids",
            }),
        }
    }

    #[instrument(skip(self))]
    pub async fn get_all_attribute_ids(&mut self) -> Result<Vec<u16>, SnapiError> {
        let mut all_ids = Vec::new();

        let mut offset = 0;

        loop {
            let mut ids = self.get_attribute_ids(offset).await?;

            let Some(id) = ids.last().copied() else {
                break;
            };

            let is_last = id == 0xFFFF;
            if is_last {
                ids.pop();
            }
            all_ids.extend_from_slice(&ids);
            if is_last {
                break;
            }

            offset = id + 1;
        }

        Ok(all_ids)
    }

    /// Get the value of an attribute by ID.
    #[instrument(skip(self))]
    pub async fn get_attribute(
        &mut self,
        attribute_id: u16,
    ) -> Result<Option<SnapiAttributeValue>, SnapiError> {
        let id_bytes = attribute_id.to_be_bytes();
        let command = [0x00, 0x06, 0x02, 0x00, id_bytes[0], id_bytes[1]];

        match self.write_attribute_command(&command).await {
            Ok(SnapiAttributeResponse::Get(value)) => Ok(value),
            Ok(_) => Err(SnapiError::MismatchedData {
                name: "attribute get",
            }),
            // Long attributes are only partially sent per request. We have to
            // make more requests to fetch the rest of the data.
            Err(SnapiError::PartialData { expected, mut got }) => {
                debug!("got partial data, requesting rest of data");

                while got.len() < expected {
                    debug!(expected, len = got.len(), "making request for next data");
                    let len_bytes = u16::try_from(got.len()).unwrap().to_be_bytes();
                    match self
                        .write_attribute_command(&[
                            0x00,
                            0x08,
                            0x04,
                            0x00,
                            id_bytes[0],
                            id_bytes[1],
                            len_bytes[0],
                            len_bytes[1],
                        ])
                        .await?
                    {
                        SnapiAttributeResponse::GetOffset(data) => got.extend_from_slice(&data),
                        _ => {
                            return Err(SnapiError::MismatchedData {
                                name: "attribute get offset",
                            });
                        }
                    }
                }

                Ok(Some(SnapiAttributeValue::Array(got)))
            }
            Err(err) => return Err(err),
        }
    }

    #[instrument(skip(self, value))]
    pub async fn set_attribute(
        &mut self,
        attribute_id: u16,
        store: bool,
        value: SnapiAttributeValue,
    ) -> Result<(), SnapiError> {
        let data = value.encode().ok_or_else(|| SnapiError::MismatchedData {
            name: "cannot encode unknown value",
        })?;

        let len = data.len() + 6;

        let mut command = Vec::with_capacity(len);
        command.extend_from_slice(
            &u16::try_from(len)
                .expect("len must fit in u16")
                .to_be_bytes(),
        );
        command.push(if store { 0x06 } else { 0x05 });
        command.push(0x00);
        command.extend_from_slice(&attribute_id.to_be_bytes());
        command.extend_from_slice(&data);

        match self.write_attribute_command(&command).await? {
            SnapiAttributeResponse::Store if store => Ok(()),
            SnapiAttributeResponse::Set if !store => Ok(()),
            _ => Err(SnapiError::MismatchedData {
                name: "attribute set",
            }),
        }
    }

    /// Perform a simple command that produces no return value, and optionally
    /// checks the status.
    #[instrument(skip(self, packet))]
    async fn write_command(
        &mut self,
        hid_output: HidOutput,
        packet: &mut [u8],
        read_status: bool,
    ) -> Result<(), SnapiError> {
        trace!("writing command: {}", hex::encode(&packet));

        debug_assert!(
            hid_output == HidOutput::from(packet[0]),
            "first byte of packet must match hid output, expected {hid_output:?}, got {:?}",
            HidOutput::from(packet[0])
        );

        if read_status {
            Arc::clone(&self.pending)
                .input_single(HidInput::Status, async move |mut rx| {
                    self.hid_device.write_report(packet).await?;

                    trace!("reading status");
                    self.read_status(&mut rx)
                        .await?
                        .error_for_status(hid_output)?;

                    Ok(())
                })
                .await?;
        } else {
            self.hid_device.write_report(packet).await?;
        }

        Ok(())
    }

    #[instrument(skip_all)]
    async fn write_attribute_command(
        &mut self,
        command: &[u8],
    ) -> Result<SnapiAttributeResponse, SnapiError> {
        let response = Arc::clone(&self.pending)
            .input_multi(HidInput::Attribute, async move |mut rx| {
                for mut packet in encode_attribute_request(command) {
                    self.write_command(HidOutput::Attribute, &mut packet, true)
                        .await?;
                }

                let value = loop {
                    trace!("waiting for attribute response");

                    let buf = rx.next().await.ok_or_else(|| SnapiError::Channel {
                        name: "attribute response",
                    })?;
                    let value = buf[0];

                    let output = self.process_packet(buf).await?;

                    match output {
                        Some(SnapiOutput::Attribute(attribute)) => break attribute,
                        Some(_) => {
                            return Err(SnapiError::UnexpectedValue {
                                value,
                                name: "attribute".into(),
                            });
                        }
                        None => continue,
                    }
                };

                Ok(value)
            })
            .await?;
        trace!("got attribute response: {response:?}");

        Ok(response)
    }

    /// Read a single status packet.
    #[instrument(skip(self))]
    async fn read_status(
        &mut self,
        rx: &mut oneshot::Receiver<Vec<u8>>,
    ) -> Result<SnapiStatus, SnapiError> {
        let buf = rx
            .await
            .map_err(|_| SnapiError::Channel { name: "rx status" })?;

        SnapiPacket::decode_specific(&buf)
    }

    /// Initialize the device.
    ///
    /// There are a couple of magic commands that must be run before the device
    /// will start talking to us.
    #[instrument(skip(self))]
    async fn initialize_device(&mut self) -> Result<(), SnapiError> {
        debug!("initializing device");
        self.write_attribute_command(&[0x00, 0x06, 0x20, 0x00, 0x04, 0xB0])
            .await?;

        Ok(())
    }

    /// Attempt to process a packet by collating all packets into an output.
    ///
    /// Many packets can be spread across multiple packets, this will take in a
    /// packet, check if it needs more data, then either store the partial value
    /// for future use or combine all of the data into a complete output.
    ///
    /// This automatically handles acknowledging packets.
    #[instrument(skip_all)]
    pub async fn process_packet(
        &mut self,
        data: Vec<u8>,
    ) -> Result<Option<SnapiOutput>, SnapiError> {
        let hid_input = HidInput::from(data[0]);
        trace!(?hid_input, "processing data");

        self.write_ack(hid_input).await?;

        let packet = SnapiPacket::decode_any(&data)?;
        trace!(?packet, "decoded packet");

        let output = match packet {
            SnapiPacket::Status(status) => status.output(),
            SnapiPacket::Barcode(packet) => packet.collate(&mut self.barcode_packets)?.output(),
            SnapiPacket::Notification(notification) => notification.output(),
            SnapiPacket::Attribute(packet) => packet.collate(&mut self.attribute_packets)?.output(),
            SnapiPacket::Other { hid_input, data } => Some(SnapiOutput::Other { hid_input, data }),
        };
        trace!(?output, "got collate output");

        Ok(output)
    }
}

impl<H: ClosableHidDevice, U> Snapi<H, U> {
    pub async fn close(mut self) -> Result<(), SnapiError> {
        self.is_connected = false;

        self.shutdown();

        if let Some(reader_ended_rx) = self.reader_ended_rx.take() {
            reader_ended_rx.await.map_err(|_| SnapiError::Channel {
                name: "reader ended",
            })?;
        }

        self.hid_device.close().await?;

        Ok(())
    }
}

impl<H: WritableHidDevice, U: UsbDevice + 'static> Snapi<H, U> {
    pub async fn attach_usb_device(
        &mut self,
        mut usb_device: U,
    ) -> Result<mpsc::Receiver<Result<SnapiData, SnapiError>>, SnapiError> {
        usb_device
            .select_configuration(1)
            .await
            .map_err(SnapiError::usb)?;

        usb_device
            .claim_interface(1)
            .await
            .map_err(SnapiError::usb)?;

        let (data_tx, data_rx) = mpsc::channel(0);

        for (address, mode) in [(0x82, SnapiMode::Image), (0x83, SnapiMode::Video)] {
            let mut endpoint = usb_device
                .claim_bulk_input_endpoint(1, address, 4096)
                .await
                .map_err(SnapiError::usb)?;

            let (cancel_tx, mut cancel_rx) = oneshot::channel();
            self.cancel_usb_tasks.insert(address, cancel_tx);

            let mut data_tx = data_tx.clone();

            crate::runtime::spawn(
                async move {
                    let mut buf = [0u8; 4096];

                    // General loop for new USB packets
                    loop {
                        let mut header = Vec::with_capacity(32);
                        let mut body = Vec::new();

                        while header.len() < 32 {
                            trace!("waiting for header data");
                            // We should only cancel when waiting for headers.
                            let fut = endpoint.transfer_in(&mut buf);
                            let len = match futures::future::select(&mut cancel_rx, fut).await {
                                Either::Left((tx, _)) => {
                                    info!("usb task cancelled");
                                    if let Ok(tx) = tx {
                                        let _ = tx.send(());
                                    }
                                    return;
                                }
                                Either::Right((res, _)) => match res {
                                    Ok(len) => len,
                                    Err(err) => {
                                        error!("usb error: {err:?}");
                                        return;
                                    }
                                },
                            };

                            let header_bytes = std::cmp::min(len, 32 - header.len());
                            header.extend_from_slice(&buf[..header_bytes]);

                            trace!(
                                packet_len = len,
                                header_len = header.len(),
                                "updated usb header information: {}",
                                hex::encode(&header)
                            );

                            if len > header_bytes {
                                trace!(
                                    additional_len = len - header_bytes,
                                    "adding additional bytes to body"
                                );
                                body.extend_from_slice(&buf[header_bytes..]);
                            }
                        }

                        let total_len: usize =
                            u32::from_le_bytes([header[0], header[1], header[2], header[3]])
                                .try_into()
                                .expect("u32 should always fit into usize");
                        debug!("expecting {total_len} bytes");
                        body.reserve_exact(total_len - body.len());

                        loop {
                            let len = match endpoint.transfer_in(&mut buf).await {
                                Ok(len) => len,
                                Err(err) => {
                                    error!("usb error: {err:?}");
                                    return;
                                }
                            };
                            body.extend_from_slice(&buf[..len]);
                            debug!(
                                len,
                                total_len,
                                body_len = body.len(),
                                finished = body.len() == total_len,
                                "read more bytes"
                            );

                            if body.len() == total_len {
                                break;
                            }
                        }

                        if data_tx
                            .send(Ok(SnapiData { mode, header, body }))
                            .await
                            .is_err()
                        {
                            error!("could not send snapi data");
                        }
                    }
                }
                .instrument(tracing::info_span!("usb_poll_loop", address)),
            );
        }

        self.usb = Some((usb_device, data_tx));

        Ok(data_rx)
    }

    pub async fn detach_usb_device(&mut self) -> Result<Option<U>, SnapiError> {
        for (_, cancel_tx) in std::mem::take(&mut self.cancel_usb_tasks) {
            let (tx, rx) = oneshot::channel();

            if cancel_tx.send(tx).is_ok() {
                let _ = rx.await;
            }
        }

        Ok(self.usb.take().map(|usb| usb.0))
    }
}

impl<H, U> Snapi<H, U> {
    fn shutdown(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            debug!("shutting down snapi device");
            // We don't care if this call fails as we're done with the device.
            let _ = shutdown_tx.send(());
        }
    }
}

impl<H, U> Drop for Snapi<H, U> {
    fn drop(&mut self) {
        self.shutdown();

        if self.is_connected {
            warn!("dropped snapi device while connected, please close first");
        }
    }
}

/// A helper for handling pending requests.
///
/// When we make certain calls, we need to make sure we use those packets and
/// prevent them from entering the general event stream. This is done by
/// registering the expected output with a channel for processing.
struct Pending<T> {
    /// The general event stream for non-reserved packets.
    others: Mutex<mpsc::Sender<T>>,
    /// A collection of subscribers for specific HID inputs.
    subscribers: Mutex<HashMap<HidInput, PendingSender<T>>>,
}

impl<T> Pending<T> {
    fn new(others: mpsc::Sender<T>) -> Arc<Self> {
        Arc::new(Self {
            others: Mutex::new(others),
            subscribers: Default::default(),
        })
    }

    /// Send some input data with the correct sender.
    ///
    /// Automatically handles dispatching between a specific subscription to
    /// certain inputs or the general event stream.
    #[instrument(skip(self, data))]
    async fn send_for_input(&self, hid_input: HidInput, data: T) -> Result<(), SnapiError> {
        if let Some(tx) = self.subscribers.lock().await.get_mut(&hid_input) {
            let single_use = tx.send(data).await?;
            trace!(single_use, "sent pending request");
        } else {
            self.others
                .lock()
                .await
                .send(data)
                .await
                .map_err(|_| SnapiError::Channel { name: "others" })?;
            trace!("sent input event");
        }

        Ok(())
    }

    /// Register an input that should only occur once.
    async fn input_single<F, Fut, Output>(
        &self,
        hid_input: HidInput,
        f: F,
    ) -> Result<Output, SnapiError>
    where
        F: FnOnce(oneshot::Receiver<T>) -> Fut,
        Fut: Future<Output = Result<Output, SnapiError>>,
    {
        trace!(?hid_input, "registering single input");
        let (tx, rx) = oneshot::channel();
        self.subscribers
            .lock()
            .await
            .insert(hid_input, PendingSender::Single(Some(tx)));
        let output = f(rx).await;
        self.subscribers.lock().await.remove(&hid_input);
        output
    }

    /// Register an input that could occur any number of times.
    async fn input_multi<F, Fut, Output>(
        &self,
        hid_input: HidInput,
        f: F,
    ) -> Result<Output, SnapiError>
    where
        F: FnOnce(mpsc::Receiver<T>) -> Fut,
        Fut: Future<Output = Result<Output, SnapiError>>,
    {
        trace!(?hid_input, "registering multiple input");
        let (tx, rx) = mpsc::channel(0);
        self.subscribers
            .lock()
            .await
            .insert(hid_input, PendingSender::Multiple(tx));
        let output = f(rx).await;
        self.subscribers.lock().await.remove(&hid_input);
        output
    }
}

/// The type of pending sender, either a channel type specifically for a single
/// packet or something that can handle many.
///
/// This is probably an unneeded optimization.
enum PendingSender<T> {
    Single(Option<oneshot::Sender<T>>),
    Multiple(mpsc::Sender<T>),
}

impl<T> PendingSender<T> {
    /// Send data the correct way.
    ///
    /// Returns if the channel is considered single-use and must be discarded
    /// after the data was sent.
    async fn send(&mut self, data: T) -> Result<bool, SnapiError> {
        let single_use = match self {
            PendingSender::Single(tx) => {
                if let Some(tx) = tx.take() {
                    tx.send(data).map_err(|_| SnapiError::Channel {
                        name: "pending single",
                    })?;
                    true
                } else {
                    return Err(SnapiError::Channel {
                        name: "already used single use pending sender",
                    });
                }
            }
            PendingSender::Multiple(tx) => {
                tx.send(data).await.map_err(|_| SnapiError::Channel {
                    name: "pending multiple",
                })?;
                false
            }
        };

        Ok(single_use)
    }
}

/// A reader for SNAPI packets.
struct SnapiReader {
    pending: Arc<Pending<Vec<u8>>>,
}

impl SnapiReader {
    /// Start a reader by spawning a new task to continuously read input
    /// report packets.
    fn start(
        packets: impl Stream<Item = Vec<u8>> + Send + 'static,
        pending: Arc<Pending<Vec<u8>>>,
        mut shutdown_rx: oneshot::Receiver<()>,
        reader_ended_tx: oneshot::Sender<()>,
    ) {
        let mut reader = Self { pending };

        crate::runtime::spawn(async move {
            let packets = packets.fuse();
            futures::pin_mut!(packets);

            loop {
                futures::select! {
                    _ = shutdown_rx => {
                        debug!("shutting down device reader task");
                        break;
                    }

                    res = packets.next() => {
                        match res {
                            Some(buf) => match reader.handle_read(buf).await {
                                Ok(_) => trace!("handled read"),
                                Err(err) => error!("could not handle read: {err}"),
                            }
                            None => {
                                error!("event stream ended");
                            }
                        }

                    }
                }
            }

            // Notify for task end if we can.
            let _ = reader_ended_tx.send(());
        });
    }

    /// Handle an incoming HID packet.
    #[instrument(skip_all)]
    async fn handle_read(&mut self, buf: Vec<u8>) -> Result<(), SnapiError> {
        let hid_input = HidInput::from(buf[0]);
        trace!(?hid_input, "read hid input data: {}", hex::encode(&buf));

        self.pending.send_for_input(hid_input, buf).await?;

        Ok(())
    }
}
