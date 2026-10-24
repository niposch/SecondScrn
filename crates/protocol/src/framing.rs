//! Length-prefixed JSON framing: `u32` big-endian length, then UTF-8 JSON.

use serde::{Serialize, de::DeserializeOwned};

/// Upper bound for a single control message. SDP is the largest message and
/// stays well below this; the limit protects against memory exhaustion.
pub const MAX_FRAME_LEN: usize = 256 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame of {0} bytes exceeds the {MAX_FRAME_LEN} byte limit")]
    TooLarge(usize),
    #[error("malformed message: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("connection closed")]
    Closed,
}

/// Serialises `msg` into a complete frame (header + body).
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let body = serde_json::to_vec(msg)?;
    if body.len() > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(body.len()));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decodes one frame body (without header).
pub fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, FrameError> {
    Ok(serde_json::from_slice(body)?)
}

#[cfg(feature = "async")]
mod io {
    use super::*;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    pub async fn write_message<W, T>(w: &mut W, msg: &T) -> Result<(), FrameError>
    where
        W: AsyncWrite + Unpin,
        T: Serialize,
    {
        let frame = encode(msg)?;
        w.write_all(&frame).await?;
        w.flush().await?;
        Ok(())
    }

    pub async fn read_message<R, T>(r: &mut R) -> Result<T, FrameError>
    where
        R: AsyncRead + Unpin,
        T: DeserializeOwned,
    {
        let mut header = [0u8; 4];
        match r.read_exact(&mut header).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(FrameError::Closed);
            }
            Err(e) => return Err(e.into()),
        }
        let len = u32::from_be_bytes(header) as usize;
        if len > MAX_FRAME_LEN {
            return Err(FrameError::TooLarge(len));
        }
        let mut body = vec![0u8; len];
        r.read_exact(&mut body).await?;
        decode(&body)
    }
}

#[cfg(feature = "async")]
pub use io::{read_message, write_message};

#[cfg(all(test, feature = "async"))]
mod tests {
    use super::*;
    use crate::{ClientMessage, HostMessage};

    #[tokio::test]
    async fn frames_roundtrip_over_a_stream() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        write_message(&mut a, &ClientMessage::Ping { nonce: 42 })
            .await
            .unwrap();
        write_message(&mut a, &ClientMessage::Goodbye)
            .await
            .unwrap();
        let m1: ClientMessage = read_message(&mut b).await.unwrap();
        let m2: ClientMessage = read_message(&mut b).await.unwrap();
        assert_eq!(m1, ClientMessage::Ping { nonce: 42 });
        assert_eq!(m2, ClientMessage::Goodbye);
    }

    #[tokio::test]
    async fn oversized_frames_are_rejected_before_allocation() {
        let (mut a, mut b) = tokio::io::duplex(64);
        use tokio::io::AsyncWriteExt;
        a.write_all(&(u32::MAX).to_be_bytes()).await.unwrap();
        let res: Result<HostMessage, _> = read_message(&mut b).await;
        assert!(matches!(res, Err(FrameError::TooLarge(_))));
    }

    #[tokio::test]
    async fn eof_is_reported_as_closed() {
        let (a, mut b) = tokio::io::duplex(64);
        drop(a);
        let res: Result<HostMessage, _> = read_message(&mut b).await;
        assert!(matches!(res, Err(FrameError::Closed)));
    }
}
