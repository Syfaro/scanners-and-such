use futures::{
    SinkExt,
    channel::{mpsc, oneshot},
    future::{Either, select},
};
use tracing::{error, info, trace, warn};

use crate::{
    scanner::snapi::{SnapiData, SnapiError, SnapiMode},
    transports::usb::UsbDeviceTransportInput,
};

pub(crate) const DEFAULT_MAX_BODY_LEN: usize = 8 * 1024 * 1024;
const HEADER_LEN: usize = 32;

type CancelRequest = oneshot::Sender<()>;

struct ImageFrame {
    header: Vec<u8>,
    body: Vec<u8>,
}

pub(crate) struct ImageAssembler {
    max_body_len: usize,
    header: Vec<u8>,
    body: Vec<u8>,
    body_len: Option<usize>,
}

impl ImageAssembler {
    pub(crate) fn new(max_body_len: usize) -> Self {
        Self {
            max_body_len,
            header: Vec::with_capacity(HEADER_LEN),
            body: Vec::new(),
            body_len: None,
        }
    }

    fn is_idle(&self) -> bool {
        self.header.is_empty()
    }

    fn push(&mut self, mut data: &[u8]) -> Result<Vec<ImageFrame>, SnapiError> {
        let mut frames = Vec::new();

        while !data.is_empty() {
            let Some(body_len) = self.body_len else {
                let needed = HEADER_LEN - self.header.len();
                let (header, rest) = data.split_at(needed.min(data.len()));
                self.header.extend_from_slice(header);
                data = rest;

                if self.header.len() == HEADER_LEN {
                    self.start_body()?;
                }

                continue;
            };

            let needed = body_len - self.body.len();
            let (body, rest) = data.split_at(needed.min(data.len()));
            self.body.extend_from_slice(body);
            data = rest;

            if self.body.len() == body_len {
                frames.push(self.take_frame());
            }
        }

        Ok(frames)
    }

    fn start_body(&mut self) -> Result<(), SnapiError> {
        let len = u32::from_le_bytes(
            self.header[..4]
                .try_into()
                .expect("header should have at least 4 bytes"),
        );
        let body_len = usize::try_from(len)
            .ok()
            .filter(|len| (1..=self.max_body_len).contains(len));

        let Some(body_len) = body_len else {
            self.header.clear();
            return Err(SnapiError::InvalidImageLength {
                len,
                max: self.max_body_len,
            });
        };

        self.body = Vec::with_capacity(body_len);
        self.body_len = Some(body_len);

        Ok(())
    }

    fn take_frame(&mut self) -> ImageFrame {
        self.body_len = None;

        ImageFrame {
            header: std::mem::replace(&mut self.header, Vec::with_capacity(HEADER_LEN)),
            body: std::mem::take(&mut self.body),
        }
    }
}

pub(crate) async fn read_endpoint<E>(
    mut endpoint: E,
    mode: SnapiMode,
    mut assembler: ImageAssembler,
    mut cancel_rx: oneshot::Receiver<CancelRequest>,
    mut data_tx: mpsc::Sender<Result<SnapiData, SnapiError>>,
) where
    E: UsbDeviceTransportInput,
    E::Error: std::fmt::Debug,
{
    let mut buf = [0u8; 4096];

    loop {
        let res = match select(&mut cancel_rx, endpoint.transfer_in(&mut buf)).await {
            Either::Left((tx, _)) => return acknowledge_cancel(tx, &assembler),
            Either::Right((res, _)) => res.map_err(SnapiError::usb),
        };
        let len = match res {
            Ok(len) => len,
            Err(err) => return fail(&mut cancel_rx, &mut data_tx, &assembler, err).await,
        };
        trace!(len, "read usb data");

        let frames = match assembler.push(&buf[..len]) {
            Ok(frames) => frames,
            Err(err) => return fail(&mut cancel_rx, &mut data_tx, &assembler, err).await,
        };

        for ImageFrame { header, body } in frames {
            let send = data_tx.send(Ok(SnapiData { mode, header, body }));
            match select(&mut cancel_rx, send).await {
                Either::Left((tx, _)) => return acknowledge_cancel(tx, &assembler),
                Either::Right((Ok(()), _)) => {}
                Either::Right((Err(_), _)) => {
                    error!("could not send snapi data");
                    return;
                }
            }
        }
    }
}

async fn fail(
    cancel_rx: &mut oneshot::Receiver<CancelRequest>,
    data_tx: &mut mpsc::Sender<Result<SnapiData, SnapiError>>,
    assembler: &ImageAssembler,
    err: SnapiError,
) {
    error!("usb reader failed: {err}");

    match select(cancel_rx, data_tx.send(Err(err))).await {
        Either::Left((tx, _)) => acknowledge_cancel(tx, assembler),
        Either::Right((Ok(()), _)) => {}
        Either::Right((Err(_), _)) => error!("could not send usb error"),
    }
}

fn acknowledge_cancel(tx: Result<CancelRequest, oneshot::Canceled>, assembler: &ImageAssembler) {
    info!("usb task cancelled");
    if !assembler.is_idle() {
        warn!("discarding partially received usb data");
    }
    if let Ok(tx) = tx {
        let _ = tx.send(());
    }
}
