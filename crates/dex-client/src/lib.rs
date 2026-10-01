//! IPC client for the DEX runtime.
//!
//! Knows the wire format and nothing else: it opens a socket, writes request
//! frames, reads response frames, and hands the caller a stream of events. It
//! has no opinion about what a session is doing and performs no repository
//! operation of its own.
//!
//! One task owns the read side. Everything the runtime sends arrives on it and
//! is routed from there: acknowledgements to whichever request is waiting, and
//! everything else to the event stream. Two readers would race, and a frontend
//! that lost an event because it was busy waiting for an acknowledgement would
//! be showing an incomplete turn.

pub mod transport;

pub use transport::{Frame, FrameReader, FrameWriter};

use dex_protocol::{
    Ack, ClientRequest, ErrorPayload, Event, EventFrame, RequestFrame, RequestId, ServerResponse,
    SessionId,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

/// The stream of events a running session produces.
pub struct EventStream {
    inner: mpsc::UnboundedReceiver<EventFrame>,
}

impl EventStream {
    /// A stream that yields nothing, standing in for the one taken.
    fn closed() -> Self {
        let (_tx, rx) = mpsc::unbounded_channel();
        Self { inner: rx }
    }

    /// The next event, or `None` once the runtime has closed the connection.
    pub async fn next(&mut self) -> Option<Event> {
        self.inner.recv().await.map(|frame| frame.event)
    }
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A connection to a running runtime.
pub struct Client<W> {
    writer: FrameWriter<W>,
    /// Acknowledgements, tagged with the request they answer.
    acks: mpsc::UnboundedReceiver<(RequestId, Result<Ack, ErrorPayload>)>,
    events: EventStream,
    next_id: RequestId,
}

impl<W> Client<W>
where
    W: AsyncWrite + Unpin + Send,
{
    /// Take a stream and start serving reads from it.
    ///
    /// `R` is the read half; `W` the write half. Splitting inside the client is
    /// what keeps a single owner on the read side.
    pub fn split<R>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        let (acks_tx, acks) = mpsc::unbounded_channel();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        tokio::spawn(forward_acks_and_events(reader, acks_tx, events_tx));

        Self {
            writer: FrameWriter::new(writer),
            acks,
            events: EventStream { inner: events_rx },
            next_id: RequestId(1),
        }
    }

    fn take_id(&mut self) -> RequestId {
        let id = self.next_id;
        self.next_id = RequestId(self.next_id.0 + 1);
        id
    }

    /// Send a request and wait for its acknowledgement.
    ///
    /// Events that arrive while waiting are routed to the event stream, so a turn
    /// that is already producing output is never stalled by a request.
    pub async fn request(
        &mut self,
        build: impl FnOnce(RequestId) -> ClientRequest,
    ) -> Result<Ack, ClientError> {
        let id = self.take_id();
        self.writer
            .send(&RequestFrame::new(id, build(id)))
            .await
            .map_err(ClientError::Transport)?;

        loop {
            match self.acks.recv().await {
                Some((ack_id, Ok(ack))) if ack_id == id => return Ok(ack),
                Some((ack_id, Err(error))) if ack_id == id => return Err(ClientError::Runtime(error)),
                // Another request's acknowledgement, or a duplicate delivery.
                Some(_) => continue,
                None => return Err(ClientError::Closed),
            }
        }
    }

    /// Open a session scoped to a working directory.
    pub async fn create_session(
        &mut self,
        working_dir: impl Into<String>,
        model: Option<String>,
    ) -> Result<SessionId, ClientError> {
        let working_dir = working_dir.into();
        match self
            .request(|_| ClientRequest::CreateSession {
                working_dir,
                model,
            })
            .await?
        {
            Ack::CreateSession { session_id, .. } => Ok(session_id),
            other => Err(ClientError::UnexpectedAck(other.kind())),
        }
    }

    /// Send a user turn. Returns once the runtime accepts it; the turn's events
    /// arrive on [`Client::next_event`].
    pub async fn send_message(
        &mut self,
        session_id: SessionId,
        text: impl Into<String>,
    ) -> Result<(), ClientError> {
        self.request(|_| ClientRequest::SendMessage {
            session_id,
            text: text.into(),
        })
        .await
        .map(|_| ())
    }

    /// Ask the runtime to cancel the turn in flight.
    pub async fn cancel(&mut self, session_id: SessionId) -> Result<(), ClientError> {
        self.request(|_| ClientRequest::Cancel { session_id })
            .await
            .map(|_| ())
    }

    /// Re-subscribe to a session after reconnecting.
    pub async fn attach(&mut self, session_id: SessionId) -> Result<(), ClientError> {
        self.request(|_| ClientRequest::Attach { session_id })
            .await
            .map(|_| ())
    }

    /// Close a session, which cancels anything it is doing.
    pub async fn close(&mut self, session_id: SessionId) -> Result<(), ClientError> {
        self.request(|_| ClientRequest::CloseSession { session_id })
            .await
            .map(|_| ())
    }

    /// What a session is permitted to do.
    pub async fn list_capabilities(&mut self) -> Result<Ack, ClientError> {
        self.request(|_| ClientRequest::ListCapabilities).await
    }

    /// The next event, or `None` once the runtime has closed the connection.
    pub async fn next_event(&mut self) -> Option<Event> {
        self.events.next().await
    }

    /// Take the event stream, so a caller can select on events and user input
    /// together without borrowing the client.
    ///
    /// Taken once: a frontend has one event stream for its lifetime, and a
    /// second consumer would silently miss everything the first took.
    pub fn take_events(&mut self) -> EventStream {
        std::mem::replace(&mut self.events, EventStream::closed())
    }
}

/// Route everything the runtime sends: acks to their request, the rest as events.
async fn forward_acks_and_events<R>(
    reader: R,
    acks: mpsc::UnboundedSender<(RequestId, Result<Ack, ErrorPayload>)>,
    events: mpsc::UnboundedSender<EventFrame>,
) where
    R: AsyncRead + Unpin,
{
    let mut reader = FrameReader::new(reader);
    loop {
        match reader.next::<ServerResponse>().await {
            Ok(Frame::Complete(ServerResponse::Ack { id, ok })) => {
                if acks.send((id, Ok(ok))).is_err() {
                    return;
                }
            }
            Ok(Frame::Complete(ServerResponse::Err { id, error })) => {
                if acks.send((id, Err(error))).is_err() {
                    return;
                }
            }
            Ok(Frame::Complete(ServerResponse::Event(frame))) => {
                if events.send(frame).is_err() {
                    return;
                }
            }
            // End of stream, or bytes that no longer parse: either way there is
            // nothing more to forward, and the acks channel closing tells the
            // caller the runtime is gone.
            Ok(Frame::Closed) | Err(_) => return,
        }
    }
}

/// Why a client operation failed.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("the socket failed: {0}")]
    Transport(std::io::Error),
    #[error("the runtime closed the connection")]
    Closed,
    #[error("{0}")]
    Runtime(ErrorPayload),
    #[error("unexpected acknowledgement: {0}")]
    UnexpectedAck(&'static str),
}

/// Why the runtime could not be reached.
#[derive(Debug)]
pub enum ConnectError {
    NoRuntime {
        source: std::io::Error,
        path: String,
    },
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::NoRuntime { source, path } => write!(
                f,
                "could not reach a runtime at {path}: {source}\n\n\
                 Start one with `dexd` in another terminal. `dex` will not start the runtime \
                 for you: quietly spawning it would hide the process boundary these two \
                 repositories exist to make visible."
            ),
        }
    }
}

impl std::error::Error for ConnectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConnectError::NoRuntime { source, .. } => Some(source),
        }
    }
}

/// Connect to a running runtime over its Unix socket.
pub async fn connect(
    path: &std::path::Path,
) -> Result<Client<tokio::net::unix::OwnedWriteHalf>, ConnectError> {
    let stream = tokio::net::UnixStream::connect(path)
        .await
        .map_err(|source| ConnectError::NoRuntime {
            source,
            path: path.display().to_string(),
        })?;
    let (reader, writer) = stream.into_split();
    Ok(Client::split(reader, writer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A client wired to an in-memory runtime that answers requests and publishes
    /// events, so the routing can be tested without a socket.
    ///
    /// Each end of the duplex needs its own read and write half: the client
    /// reads responses and writes requests, the runtime the reverse.
    fn client() -> Client<tokio::io::WriteHalf<tokio::io::DuplexStream>> {
        let (client_end, runtime_end) = tokio::io::duplex(64 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_end);
        let (mut runtime_reader, mut runtime_writer) = tokio::io::split(runtime_end);

        tokio::spawn(async move {
            while let Ok(Frame::Complete(frame)) =
                FrameReader::new(&mut runtime_reader).next::<RequestFrame>().await
            {
                // Acknowledge, then publish an event alongside it: the order the
                // runtime really uses, and the one this client must not lose.
                let response = match &frame.request {
                    ClientRequest::CreateSession { model, .. } => ServerResponse::ack(
                        frame.id,
                        Ack::CreateSession {
                            session_id: SessionId::new(),
                            status: dex_protocol::SessionStatus::Created,
                            model: model.clone().unwrap_or_else(|| "test".into()),
                        },
                    ),
                    _ => ServerResponse::ack(frame.id, Ack::Accepted),
                };
                let mut writer = FrameWriter::new(&mut runtime_writer);
                writer.send(&response).await.expect("ack");
                writer
                    .send(&ServerResponse::Event(EventFrame::new(
                        SessionId::new(),
                        1,
                        Event::ModelDelta {
                            text: "program source".into(),
                        },
                    )))
                    .await
                    .expect("event");
            }
        });

        Client::split(client_reader, client_writer)
    }

    #[tokio::test]
    async fn an_acknowledgement_reaches_the_caller() {
        let mut c = client();
        let ack = tokio::time::timeout(Duration::from_secs(5), c.request(|_| ClientRequest::ListCapabilities))
            .await
            .expect("must not hang")
            .expect("acked");
        assert!(matches!(ack, Ack::Accepted));
    }

    #[tokio::test]
    async fn creating_a_session_returns_its_id() {
        let mut c = client();
        let id = c
            .create_session("/work", None)
            .await
            .expect("created");
        // Two sessions differ.
        let other = c.create_session("/work", None).await.expect("created");
        assert_ne!(id, other);
    }

    #[tokio::test]
    async fn events_are_delivered_even_while_a_request_is_pending() {
        let mut c = client();
        let ack = c.request(|_| ClientRequest::ListCapabilities).await.expect("acked");
        assert!(matches!(ack, Ack::Accepted));
        // The event the runtime published alongside the ack is not lost just
        // because a request was being awaited.
        let event = tokio::time::timeout(Duration::from_secs(5), c.next_event())
            .await
            .expect("event should arrive")
            .expect("some event");
        assert!(matches!(event, Event::ModelDelta { .. }));
    }
}