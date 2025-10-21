use std::{borrow::Cow, collections::HashMap, sync::Arc};

use async_hid::{AsyncHidRead, AsyncHidWrite, DeviceReader, DeviceWriter};
use futures::{
    FutureExt, SinkExt, StreamExt,
    channel::{mpsc, oneshot},
    future,
    lock::Mutex,
};
use num_enum::{FromPrimitive, IntoPrimitive};
use tap::TapFallible;
use tracing::{Instrument, debug, error, instrument, trace, warn};

use crate::scanner::snapi::packet::*;

pub mod code_types;
pub mod packet;

pub const USB_VENDOR_ID: u16 = 0x05E0;
pub const USB_PRODUCT_ID: u16 = 0x1900;

pub const ATTRIBUTE_SERIAL_NUMBER: u16 = 534;

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, IntoPrimitive, FromPrimitive)]
#[repr(u8)]
pub enum HidInput {
    Status = 0x21,
    Barcode = 0x22,
    Attribute = 0x27,
    #[num_enum(catch_all)]
    Unknown(u8),
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, IntoPrimitive, FromPrimitive)]
#[repr(u8)]
pub enum HidOutput {
    Attribute = 0x0D,
    #[num_enum(catch_all)]
    Unknown(u8),
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
    #[error("encountered error when using channel: {name}")]
    Channel { name: &'static str },
    #[error("packets were missing or out of order: {name}")]
    BadPacketOrder { name: Cow<'static, str> },
}

#[derive(Debug)]
pub struct SnapiDeviceInfo {
    pub name: String,
    pub serial_number: Option<String>,
    device: async_hid::Device,
}

impl SnapiDeviceInfo {
    pub async fn open(self) -> Result<(Snapi, mpsc::Receiver<Vec<u8>>), SnapiError> {
        Snapi::new(&self.device).await
    }
}

/// A SNAPI device.
pub struct Snapi {
    wtr: DeviceWriter,
    pending: Pending<Vec<u8>>,
    shutdown_tx: Option<oneshot::Sender<()>>,

    barcode_packets: Vec<SnapiBarcodePacket>,
    attribute_packets: Vec<SnapiAttributePacket>,
}

impl Snapi {
    /// Discover SNAPI devices on the system.
    ///
    /// If `read_serial_numbers` is enabled, it will connect and initialize each
    /// device found to retrieve the serial number.
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
                    device.vendor_id == USB_VENDOR_ID && device.product_id == USB_PRODUCT_ID,
                )
            })
            .filter_map(|device| {
                async move {
                    debug!(id = ?device.id, "found device");

                    if read_serial_numbers {
                        let name = device.name.clone();

                        let (mut snapi_device, _) = Self::new(&device)
                            .await
                            .tap_err(|err| warn!("could not open snapi device: {err}"))
                            .ok()?;

                        let serial_number = snapi_device
                            .get_attribute(ATTRIBUTE_SERIAL_NUMBER)
                            .await
                            .tap_err(|err| warn!("could not get serial number: {err}"))
                            .ok()?;

                        match serial_number {
                            Some(SnapiAttributeValue::String(serial_number)) => {
                                Some(SnapiDeviceInfo {
                                    name,
                                    serial_number: Some(serial_number),
                                    device,
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
                            device,
                        })
                    }
                }
                .in_current_span()
            })
            .collect()
            .await;

        Ok(devices)
    }

    /// Create a new SNAPI device from a HID device and initializes it.
    async fn new(
        device: &async_hid::Device,
    ) -> Result<(Self, mpsc::Receiver<Vec<u8>>), SnapiError> {
        let (rdr, wtr) = device.open().await?;

        let (others_tx, others_rx) = mpsc::channel(8);
        let pending = Pending::new(others_tx);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let mut snapi_device = Self {
            wtr,
            pending: pending.clone(),
            shutdown_tx: Some(shutdown_tx),

            barcode_packets: Vec::new(),
            attribute_packets: Vec::new(),
        };

        // Spawn reader after creating device, because now device will send a
        // message to the shutdown channel when dropped.
        SnapiReader::start(rdr, pending, shutdown_rx);

        snapi_device.initialize_device().await?;

        Ok((snapi_device, others_rx))
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

    /// Get the value of an attribute by ID.
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

    /// Encodes an attribute command into as many packets are as needed and send
    /// them all.
    #[instrument(skip_all)]
    async fn write_attribute_command(
        &mut self,
        command: &[u8],
    ) -> Result<Option<SnapiAttributeValue>, SnapiError> {
        for packet in encode_attribute_request(command) {
            trace!("writing command packet: {}", hex::encode(&packet));
            self.wtr.write_output_report(&packet).await?;
            self.read_status().await?.error_for_status()?;
        }

        self.read_attribute().await
    }

    /// Write an acknowledgement packet.
    ///
    /// Note that is only needed if you're not using the
    /// [`Snapi::process_packet`] function, as that handles this automatically.
    #[instrument(skip(self, input))]
    pub async fn write_ack(&mut self, input: HidInput) -> Result<(), SnapiError> {
        let packet = [0x01, input.into(), 0x01];
        trace!(?input, "writing ack: {}", hex::encode(packet));
        self.wtr.write_output_report(&packet).await?;

        Ok(())
    }

    /// Read a single status packet.
    #[instrument(skip(self))]
    async fn read_status(&mut self) -> Result<SnapiStatus, SnapiError> {
        let pending = self.pending.clone();
        let buf = pending
            .input_single(self, HidInput::Status, |_, rx| async move {
                rx.await
                    .map_err(|_| SnapiError::Channel { name: "rx status" })
            })
            .await?;

        SnapiPacket::decode_specific(&buf)
    }

    /// Reads an attribute value.
    #[instrument(skip(self))]
    async fn read_attribute(&mut self) -> Result<Option<SnapiAttributeValue>, SnapiError> {
        let pending = self.pending.clone();
        let value = pending
            .input_multi(self, HidInput::Attribute, |device, mut rx| {
                async move {
                    loop {
                        trace!("waiting for attribute response");

                        let buf = rx.next().await.ok_or_else(|| SnapiError::Channel {
                            name: "attribute response",
                        })?;
                        let value = buf[0];

                        let output = device.process_packet(buf).await?;

                        match output {
                            Some(SnapiOutput::Attribute(attribute)) => return Ok(attribute),
                            Some(_) => {
                                return Err(SnapiError::UnexpectedValue {
                                    value,
                                    name: "attribute".into(),
                                });
                            }
                            None => continue,
                        }
                    }
                }
                .in_current_span()
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

impl Drop for Snapi {
    fn drop(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            debug!("shutting down snapi device");
            // We don't care if this call fails as we're done with the device.
            let _ = shutdown_tx.send(());
        }
    }
}

/// A helper for handling pending requests.
///
/// When we make certain calls, we need to make sure we use those packets and
/// prevent them from entering the general event stream. This is done by
/// registering the expected output with a channel for processing.
#[derive(Clone)]
struct Pending<T> {
    others: Arc<Mutex<mpsc::Sender<T>>>,
    pending: Arc<Mutex<HashMap<HidInput, PendingSender<T>>>>,
}

impl<T> Pending<T> {
    fn new(others: mpsc::Sender<T>) -> Self {
        Self {
            others: Arc::new(Mutex::new(others)),
            pending: Default::default(),
        }
    }

    /// Send some input data with the correct sender.
    ///
    /// Automatically handles dispatching between a specific subscription to
    /// certain inputs or the general event stream.
    async fn send_for_input(&self, hid_input: HidInput, data: T) -> Result<(), SnapiError> {
        let mut pending = self.pending.lock().await;
        if let Some(tx) = pending.get_mut(&hid_input) {
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
    async fn input_single<'a, F, Fut, O>(
        &self,
        device: &'a mut Snapi,
        hid_input: HidInput,
        f: F,
    ) -> O
    where
        F: FnOnce(&'a mut Snapi, oneshot::Receiver<T>) -> Fut,
        Fut: Future<Output = O>,
    {
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .await
            .insert(hid_input, PendingSender::Single(Some(tx)));
        let output = f(device, rx).await;
        self.pending.lock().await.remove(&hid_input);
        output
    }

    /// Register an input that could occur any number of times.
    async fn input_multi<'a, F, Fut, O>(
        &self,
        device: &'a mut Snapi,
        hid_input: HidInput,
        f: F,
    ) -> O
    where
        F: FnOnce(&'a mut Snapi, mpsc::Receiver<T>) -> Fut,
        Fut: Future<Output = O>,
    {
        let (tx, rx) = mpsc::channel(0);
        self.pending
            .lock()
            .await
            .insert(hid_input, PendingSender::Multiple(tx));
        let output = f(device, rx).await;
        self.pending.lock().await.remove(&hid_input);
        output
    }
}

/// The type of pending sender, either a channel type specifically for single
/// events or something that can handle many.
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

/// A reader for SNAPI events.
struct SnapiReader {
    buf: [u8; PACKET_LEN],
    pending: Pending<Vec<u8>>,
}

impl SnapiReader {
    /// Start a reader by spawning a new task to continuously read input
    /// reports.
    fn start(
        mut rdr: DeviceReader,
        pending: Pending<Vec<u8>>,
        mut shutdown_rx: oneshot::Receiver<()>,
    ) {
        let mut reader = Self {
            buf: [0u8; PACKET_LEN],
            pending,
        };

        crate::runtime::spawn(async move {
            loop {
                let mut read_fut = Box::pin(rdr.read_input_report(&mut reader.buf).fuse());

                futures::select! {
                    _ = shutdown_rx => {
                        debug!("shutting down device reader task");
                        break;
                    }

                    res = read_fut => {
                        drop(read_fut);

                        if let Err(err) = reader.handle_read(res).await {
                            error!("handling hid read call failed: {err}");
                        }
                    }
                }
            }
        });
    }

    /// Handle an incoming HID packet.
    #[instrument(skip_all, fields(hid_input))]
    async fn handle_read(&mut self, res: async_hid::HidResult<usize>) -> Result<(), SnapiError> {
        let len = res?;
        let data = self.buf[0..len].to_vec();

        let hid_input = HidInput::from(data[0]);
        tracing::Span::current().record("hid_input", format!("{hid_input:?}"));

        trace!(len, "read hid input: {}", hex::encode(&data));

        self.pending.send_for_input(hid_input, data).await?;

        Ok(())
    }
}
