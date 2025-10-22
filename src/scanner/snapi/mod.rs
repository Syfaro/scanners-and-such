use std::{borrow::Cow, collections::HashMap, sync::Arc};

use futures::{
    SinkExt, Stream, StreamExt,
    channel::{mpsc, oneshot},
};
use num_enum::{FromPrimitive, IntoPrimitive};
use serde::Serialize;
use tracing::{debug, error, instrument, trace, warn};

use crate::{
    scanner::snapi::packet::*,
    transports::hid::{ClosableHidDevice, HidError, WritableHidDevice},
};

pub mod code_types;
pub mod packet;

pub const USB_VENDOR_ID: u16 = 0x05E0;
pub const USB_PRODUCT_ID: u16 = 0x1900;

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, IntoPrimitive, FromPrimitive, Serialize)]
#[serde(rename_all = "camelCase")]
#[repr(u8)]
pub enum HidInput {
    Status = 0x21,
    Barcode = 0x22,
    Attribute = 0x27,
    #[num_enum(catch_all)]
    Unknown(u8),
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, IntoPrimitive, FromPrimitive, Serialize)]
#[serde(rename_all = "camelCase")]
#[repr(u8)]
pub enum HidOutput {
    Acknowledgement = 0x01,
    Aim = 0x02,
    Scanner = 0x06,
    MacroPdf = 0x08,
    Attribute = 0x0D,
    #[num_enum(catch_all)]
    Unknown(u8),
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
}

/// A SNAPI device.
pub struct Snapi<W> {
    wtr: W,
    is_connected: bool,

    pending: Arc<Pending<Vec<u8>>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    reader_ended_rx: Option<oneshot::Receiver<()>>,

    barcode_packets: Vec<SnapiBarcodePacket>,
    attribute_packets: Vec<SnapiAttributePacket>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoPrimitive)]
#[repr(u8)]
pub enum MacroPdfAction {
    Flush = 0x00,
    Abort = 0x01,
}

impl<W: WritableHidDevice> Snapi<W> {
    /// Create a new SNAPI device from a HID device and initialize it.
    pub async fn new(
        wtr: W,
        packets: impl Stream<Item = Vec<u8>> + Send + 'static,
    ) -> Result<(Self, mpsc::Receiver<Vec<u8>>), SnapiError> {
        let (others_tx, others_rx) = mpsc::channel(8);
        let pending = Pending::new(others_tx);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (reader_ended_tx, reader_ended_rx) = oneshot::channel();

        let mut snapi_device = Self {
            wtr,
            is_connected: true,

            pending: Arc::clone(&pending),
            shutdown_tx: Some(shutdown_tx),
            reader_ended_rx: Some(reader_ended_rx),

            barcode_packets: Vec::new(),
            attribute_packets: Vec::new(),
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
                    self.wtr.write_report(packet).await?;

                    trace!("reading status");
                    self.read_status(&mut rx)
                        .await?
                        .error_for_status(hid_output)?;

                    Ok(())
                })
                .await?;
        } else {
            self.wtr.write_report(packet).await?;
        }

        Ok(())
    }

    /// Get the value of an attribute by ID.
    #[instrument(skip(self))]
    pub async fn get_attribute(
        &mut self,
        attribute_id: u16,
    ) -> Result<Option<SnapiAttributeValue>, SnapiError> {
        let mut command = Vec::with_capacity(6);
        command.extend_from_slice(&[0x00, 0x06, 0x02, 0x00]);
        command.extend_from_slice(&attribute_id.to_be_bytes());

        let value = self.write_attribute_command(&command).await?;

        Ok(value)
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

    /// Encodes an attribute command into as many packets are as needed and send
    /// them all.
    #[instrument(skip_all)]
    async fn write_attribute_command(
        &mut self,
        command: &[u8],
    ) -> Result<Option<SnapiAttributeValue>, SnapiError> {
        let value = Arc::clone(&self.pending)
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

        trace!("got attribute value: {value:?}");

        Ok(value)
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
            SnapiPacket::Attribute(packet) => packet.collate(&mut self.attribute_packets)?.output(),
            SnapiPacket::Other { hid_input, data } => Some(SnapiOutput::Other { hid_input, data }),
        };
        trace!(?output, "got collate output");

        Ok(output)
    }
}

impl<W: ClosableHidDevice> Snapi<W> {
    pub async fn close(mut self) -> Result<(), SnapiError> {
        self.is_connected = false;

        self.shutdown();

        if let Some(reader_ended_rx) = self.reader_ended_rx.take() {
            reader_ended_rx.await.map_err(|_| SnapiError::Channel {
                name: "reader ended",
            })?;
        }

        self.wtr.close().await?;

        Ok(())
    }
}

impl<W> Snapi<W> {
    fn shutdown(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            debug!("shutting down snapi device");
            // We don't care if this call fails as we're done with the device.
            let _ = shutdown_tx.send(());
        }
    }
}

impl<W> Drop for Snapi<W> {
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
    others: futures::lock::Mutex<mpsc::Sender<T>>,
    /// A collection of subscribers for specific HID inputs.
    subscribers: futures::lock::Mutex<HashMap<HidInput, PendingSender<T>>>,
}

impl<T> Pending<T> {
    fn new(others: mpsc::Sender<T>) -> Arc<Self> {
        Arc::new(Self {
            others: futures::lock::Mutex::new(others),
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
