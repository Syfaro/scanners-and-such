use futures::{SinkExt, Stream, StreamExt, channel::mpsc};
use serde::{Deserialize, Serialize};
use tracing::{error, trace, warn};

use crate::transports::hid::HidDevice;

pub const PACKET_LEN: usize = 64;

pub struct HidPos<H> {
    hid_device: H,
}

#[cfg_attr(feature = "web", derive(tsify::Tsify))]
#[cfg_attr(feature = "web", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HidData {
    pub symbology_id: [u8; 3],
    pub value: Vec<u8>,
}

impl<H: HidDevice> HidPos<H> {
    pub async fn new(
        hid: H,
        packets: impl Stream<Item = Vec<u8>> + Send + 'static,
    ) -> (Self, mpsc::Receiver<HidData>) {
        let pos = Self { hid_device: hid };

        let (mut tx, rx) = mpsc::channel(1);

        crate::runtime::spawn(async move {
            let mut packets = Box::pin(packets);

            let mut buf: Option<Vec<u8>> = None;

            while let Some(packet) = packets.next().await {
                if packet.len() != 64 || packet[0] != 0x02 {
                    warn!("unexpected packet data: {}", hex::encode(packet));
                    continue;
                }

                let data_len = packet[1];
                let symbology_id = &packet[2..5];
                let new_data = &packet[5..5 + usize::from(data_len)];
                let vendor_data = &packet[61..63];
                let continues = packet[63];
                trace!(
                    data_len,
                    ?symbology_id,
                    ?vendor_data,
                    continues,
                    "got data: {}",
                    hex::encode(new_data)
                );

                let mut data = buf.take().unwrap_or_default();
                data.extend_from_slice(new_data);

                if continues > 0 {
                    buf = Some(data);
                    continue;
                }

                let hid_data = HidData {
                    symbology_id: symbology_id.try_into().unwrap(),
                    value: data,
                };

                if let Err(err) = tx.send(hid_data).await {
                    error!("could not send hid data: {err}");
                }
            }
        });

        (pos, rx)
    }

    pub async fn close(mut self) -> Result<(), H::Error> {
        self.hid_device.close().await
    }
}
