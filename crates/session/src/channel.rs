//! The TLS control channel: a reader task feeding an mpsc queue (so reads are
//! cancel-safe inside `select!`) plus an inline writer.

use std::time::Duration;

use secondscrn_protocol::framing::{FrameError, read_message, write_message};
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncWrite, WriteHalf};
use tokio::sync::mpsc;

use crate::SessionError;

/// Maximum time the peer may stay silent. Both sides ping every
/// [`PING_INTERVAL`], so this tolerates a couple of lost/late pings.
pub const PEER_TIMEOUT: Duration = Duration::from_secs(8);
pub const PING_INTERVAL: Duration = Duration::from_secs(2);
/// Timeout for steps of the handshake that need no human interaction.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(15);

pub struct ControlChannel<S, In> {
    pub rx: mpsc::Receiver<Result<In, FrameError>>,
    writer: WriteHalf<S>,
    reader: tokio::task::JoinHandle<()>,
}

impl<S, In> ControlChannel<S, In>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
    In: DeserializeOwned + Send + 'static,
{
    pub fn new(stream: S) -> Self {
        let (mut r, writer) = tokio::io::split(stream);
        let (tx, rx) = mpsc::channel(256);
        let reader = tokio::spawn(async move {
            loop {
                let msg = read_message::<_, In>(&mut r).await;
                let stop = msg.is_err();
                if tx.send(msg).await.is_err() || stop {
                    return;
                }
            }
        });
        ControlChannel { rx, writer, reader }
    }

    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<(), SessionError> {
        tokio::time::timeout(STEP_TIMEOUT, write_message(&mut self.writer, msg))
            .await
            .map_err(|_| SessionError::Timeout("sending a control message"))??;
        Ok(())
    }

    /// Receives the next message, failing on timeout, EOF or decode errors.
    pub async fn recv_timeout(
        &mut self,
        timeout: Duration,
        what: &'static str,
    ) -> Result<In, SessionError> {
        match tokio::time::timeout(timeout, self.rx.recv()).await {
            Err(_) => Err(SessionError::Timeout(what)),
            Ok(None) => Err(SessionError::ConnectionClosed),
            Ok(Some(Err(FrameError::Closed))) => Err(SessionError::ConnectionClosed),
            Ok(Some(Err(e))) => Err(e.into()),
            Ok(Some(Ok(m))) => Ok(m),
        }
    }
}

impl<S, In> Drop for ControlChannel<S, In> {
    fn drop(&mut self) {
        self.reader.abort();
    }
}
