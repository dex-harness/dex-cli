//! Length-prefixed frame reading and writing over a byte stream.
//!
//! Binary, not JSON: a `u32` little-endian length followed by a `postcard`
//! payload. The prefix is what lets a reader tell a complete frame from a
//! partial one, so a frame split across TCP segments is reassembled rather than
//! mis-parsed.
//!
//! This is deliberately a copy of the runtime's transport rather than a shared
//! helper. The two repositories must not depend on each other; `dex-protocol`
//! is the vocabulary they share, and each side owns how bytes move.

use std::io;

use dex_protocol::{MAX_FRAME_BYTES, decode_payload, encode_frame, split_frame};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// What one read produced.
#[derive(Debug)]
pub enum Frame<T> {
    Complete(T),
    /// The stream ended cleanly between frames.
    Closed,
}

/// Reads frames from a stream.
pub struct FrameReader<R> {
    inner: R,
    buffer: Vec<u8>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            buffer: Vec::with_capacity(8 * 1024),
        }
    }

    /// Read the next frame, or `Closed` at end of stream.
    ///
    /// A malformed frame is an error rather than something to guess at: the
    /// stream position is no longer trustworthy once the bytes do not parse.
    pub async fn next<T: serde::de::DeserializeOwned>(&mut self) -> Result<Frame<T>, io::Error> {
        loop {
            match split_frame(&self.buffer) {
                Ok(Some((header, payload))) => {
                    let value = decode_payload::<T>(payload).map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidData, e.to_string())
                    })?;
                    let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]])
                        as usize;
                    self.buffer.drain(..4 + len);
                    return Ok(Frame::Complete(value));
                }
                Ok(None) => {}
                Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
            }

            if self.buffer.len() > MAX_FRAME_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "frame exceeded the size limit",
                ));
            }

            let mut chunk = [0u8; 16 * 1024];
            let read = self.inner.read(&mut chunk).await?;
            if read == 0 {
                return Ok(Frame::Closed);
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }
}

/// Writes frames to a stream.
///
/// Generic over the payload so the same writer carries requests and responses;
/// the framing is identical either way.
pub struct FrameWriter<W> {
    inner: W,
}

impl<W: AsyncWrite + Unpin> FrameWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    pub async fn send<T: serde::Serialize>(&mut self, message: &T) -> io::Result<()> {
        let framed = encode_frame(message)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        self.inner.write_all(&framed).await?;
        self.inner.flush().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dex_protocol::{
        Ack, Event, EventFrame, RequestFrame, RequestId, ServerResponse, SessionId,
    };

    fn frames() -> Vec<ServerResponse> {
        vec![
            ServerResponse::ack(RequestId(1), Ack::Ok),
            ServerResponse::ack(RequestId(2), Ack::Accepted),
            ServerResponse::Event(EventFrame::new(
                SessionId::new(),
                7,
                Event::ModelDelta {
                    text: "a longer streamed chunk, to cross a chunk boundary".into(),
                },
            )),
        ]
    }

    #[tokio::test]
    async fn frames_survive_a_write_and_read() {
        let expected = frames();
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        let mut writer = FrameWriter::new(&mut client);
        for response in &expected {
            writer.send(response).await.expect("send");
        }

        let mut reader = FrameReader::new(&mut server);
        for want in expected {
            match reader.next::<ServerResponse>().await.expect("read") {
                Frame::Complete(got) => assert_eq!(got, want),
                Frame::Closed => panic!("closed early"),
            }
        }
    }

    #[tokio::test]
    async fn a_frame_split_one_byte_at_a_time_is_reassembled() {
        let response = ServerResponse::ack(RequestId(9), Ack::Accepted);
        let framed = encode_frame(&response).expect("frame");

        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        let handle = tokio::spawn(async move {
            let mut reader = FrameReader::new(&mut server);
            reader.next::<ServerResponse>().await
        });
        for byte in &framed {
            client.write_all(&[*byte]).await.expect("write");
        }
        client.flush().await.expect("flush");
        drop(client);

        match handle.await.expect("join").expect("read") {
            Frame::Complete(got) => assert_eq!(got, response),
            Frame::Closed => panic!("closed early"),
        }
    }

    #[tokio::test]
    async fn a_hostile_length_prefix_is_rejected_without_allocating() {
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        client
            .write_all(&u32::MAX.to_le_bytes())
            .await
            .expect("write");
        let mut reader = FrameReader::new(&mut server);
        let err = reader.next::<ServerResponse>().await.expect_err("must reject");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn the_reader_is_generic_over_the_message_type() {
        let request = RequestFrame::new(RequestId(1), dex_protocol::ClientRequest::ListCapabilities);
        let framed = encode_frame(&request).expect("frame");

        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        let handle = tokio::spawn(async move {
            let mut reader = FrameReader::new(&mut server);
            reader.next::<RequestFrame>().await
        });
        client.write_all(&framed).await.expect("write");
        client.flush().await.expect("flush");
        drop(client);

        match handle.await.expect("join").expect("read") {
            Frame::Complete(got) => assert_eq!(got, request),
            Frame::Closed => panic!("closed early"),
        }
    }
}