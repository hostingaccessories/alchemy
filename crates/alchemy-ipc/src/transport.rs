//! Async transport helpers shared by `alchemyd` and its clients.
//!
//! Generic over [`tokio::io::AsyncRead`] / [`tokio::io::AsyncWrite`];
//! socket binding stays in the binaries.

use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{decode_message, encode_frame, encode_message, error::IpcError, frame::FrameDecoder};

/// Per-`read` scratch buffer size.
const READ_CHUNK: usize = 4 * 1024;

/// Serialise `value` to JSON, length-prefix it (capped at `max`), and
/// write it to `writer`.
pub async fn write_message<W, T>(writer: &mut W, value: &T, max: usize) -> Result<(), IpcError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let payload = encode_message(value)?;
    let frame = encode_frame(&payload, max)?;
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

/// Pull one full frame from `reader`, using `decoder` to buffer partial
/// reads and surplus bytes from pipelined frames.
///
/// Returns `Ok(None)` on a clean EOF with nothing buffered and
/// [`IpcError::UnexpectedEof`] on EOF mid-frame.
pub async fn read_frame<R>(
    reader: &mut R,
    decoder: &mut FrameDecoder,
) -> Result<Option<Vec<u8>>, IpcError>
where
    R: AsyncRead + Unpin,
{
    loop {
        if let Some(payload) = decoder.next_frame()? {
            return Ok(Some(payload));
        }
        let mut chunk = [0_u8; READ_CHUNK];
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            return if decoder.buffered() == 0 {
                Ok(None)
            } else {
                Err(IpcError::UnexpectedEof {
                    bytes: decoder.buffered(),
                })
            };
        }
        if let Some(slice) = chunk.get(..n) {
            decoder.extend_from_slice(slice);
        }
    }
}

/// [`read_frame`] followed by [`decode_message`].
pub async fn read_message<R, T>(
    reader: &mut R,
    decoder: &mut FrameDecoder,
) -> Result<Option<T>, IpcError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    match read_frame(reader, decoder).await? {
        Some(bytes) => Ok(Some(decode_message::<T>(&bytes)?)),
        None => Ok(None),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use crate::{DEFAULT_MAX_FRAME, Request};
    use tokio::io::{AsyncWriteExt, duplex};

    #[tokio::test]
    async fn write_then_read_one_message_over_duplex() {
        let (mut a, mut b) = duplex(1024);
        write_message(&mut a, &Request::Version, DEFAULT_MAX_FRAME)
            .await
            .unwrap();
        a.shutdown().await.unwrap();
        drop(a);

        let mut dec = FrameDecoder::new();
        let got: Option<Request> = read_message(&mut b, &mut dec).await.unwrap();
        assert_eq!(got, Some(Request::Version));
        let eof: Option<Request> = read_message(&mut b, &mut dec).await.unwrap();
        assert!(eof.is_none());
    }

    #[tokio::test]
    async fn write_survives_partial_writes_on_small_duplex() {
        let (mut a, mut b) = duplex(4);
        let writer = tokio::spawn(async move {
            write_message(&mut a, &Request::Ping, DEFAULT_MAX_FRAME)
                .await
                .unwrap();
            a.shutdown().await.unwrap();
        });
        let mut dec = FrameDecoder::new();
        let got: Option<Request> = read_message(&mut b, &mut dec).await.unwrap();
        writer.await.unwrap();
        assert_eq!(got, Some(Request::Ping));
    }

    #[tokio::test]
    async fn eof_mid_frame_is_unexpected_eof() {
        let (mut a, mut b) = duplex(64);
        let frame =
            encode_frame(&encode_message(&Request::Ping).unwrap(), DEFAULT_MAX_FRAME).unwrap();
        a.write_all(&frame[..4]).await.unwrap();
        a.shutdown().await.unwrap();
        drop(a);

        let mut dec = FrameDecoder::new();
        let err = read_message::<_, Request>(&mut b, &mut dec)
            .await
            .unwrap_err();
        assert!(
            matches!(err, IpcError::UnexpectedEof { bytes: 4 }),
            "got {err:?}"
        );
    }
}
