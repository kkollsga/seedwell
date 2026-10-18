//! A small async client for the control socket, shared by the CLI and tests.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::limits::{LineRead, read_line};
use crate::proto::*;
use crate::types::{EventDto, HelloDto};

/// Largest response line a client accepts.
const MAX_RESPONSE: usize = 64 << 20;

/// Why a call failed.
#[derive(Debug)]
pub enum ClientError {
    /// Nothing is listening: the app (or daemon) is not running, or the path is wrong.
    NotRunning {
        /// The socket path tried.
        path: PathBuf,
        /// The OS error.
        source: io::Error,
    },
    /// The server refused or failed the call.
    Api(ApiError),
    /// Socket I/O failed mid-conversation.
    Io(io::Error),
    /// The server sent something that is not this protocol.
    Protocol(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRunning { path, source } => match source.kind() {
                io::ErrorKind::PermissionDenied => write!(f, "cannot open {}: permission denied", path.display()),
                _ => write!(
                    f,
                    "Seedwell is not running (nothing is listening on {}). Start the app, or `seedwell daemon`.",
                    path.display()
                ),
            },
            Self::Api(e) => write!(f, "{}", e.message),
            Self::Io(e) => write!(f, "connection to Seedwell failed: {e}"),
            Self::Protocol(m) => write!(f, "unexpected answer from the server: {m}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<ApiError> for ClientError {
    fn from(e: ApiError) -> Self {
        Self::Api(e)
    }
}

/// One connection, one request at a time.
pub struct Client {
    rd: BufReader<OwnedReadHalf>,
    wr: OwnedWriteHalf,
    next_id: u64,
    events: VecDeque<EventDto>,
    buf: Vec<u8>,
    /// What `hello` returned.
    pub hello: HelloDto,
}

impl Client {
    /// Connect and say hello as `client_name`. Fails with [`ClientError::NotRunning`] when
    /// nothing listens, and with the server's error (for example `agent_access_off`) when it
    /// refuses.
    pub async fn connect(path: impl AsRef<Path>, client_name: &str) -> Result<Client, ClientError> {
        let path = path.as_ref();
        let stream = UnixStream::connect(path)
            .await
            .map_err(|source| ClientError::NotRunning { path: path.to_path_buf(), source })?;
        let (rd, wr) = stream.into_split();
        let mut c = Client {
            rd: BufReader::new(rd),
            wr,
            next_id: 1,
            events: VecDeque::new(),
            buf: Vec::new(),
            hello: HelloDto::default(),
        };
        let v = c
            .call(&Request::Hello(HelloParams {
                client: client_name.to_owned(),
                protocol: Some(PROTOCOL_VERSION),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }))
            .await?;
        c.hello = serde_json::from_value(v).map_err(|e| ClientError::Protocol(e.to_string()))?;
        Ok(c)
    }

    /// Read one message. Cancel-safe (used under a timeout): a partly read line stays in
    /// `buf` and the next call continues it.
    async fn read_message(&mut self) -> Result<ServerLine, ClientError> {
        match read_line(&mut self.rd, &mut self.buf, MAX_RESPONSE).await? {
            LineRead::Line => {
                let parsed = serde_json::from_slice(&self.buf).map_err(|e| ClientError::Protocol(e.to_string()));
                self.buf.clear();
                parsed
            }
            LineRead::Eof => Err(ClientError::Io(io::Error::new(io::ErrorKind::UnexpectedEof, "the server closed the connection"))),
            LineRead::TooLong => Err(ClientError::Protocol("response too large".into())),
        }
    }

    /// Send a request and wait for its answer. Events that arrive meanwhile are queued for
    /// [`Client::next_event`].
    pub async fn call(&mut self, req: &Request) -> Result<Value, ClientError> {
        let id = Id::Num(self.next_id);
        self.next_id += 1;
        if let Err(e) = self.wr.write_all(&encode_request(&id, req)).await {
            // A server that refused us (access off, too many clients) writes its reason and
            // hangs up, so our write can fail while the explanation is waiting to be read.
            if let Ok(Ok(m)) = tokio::time::timeout(Duration::from_secs(1), self.read_message()).await
                && let Some(err) = m.error
            {
                return Err(ClientError::Api(err));
            }
            return Err(ClientError::Io(e));
        }
        loop {
            let m = self.read_message().await?;
            if m.event.is_some() {
                if let Some(e) = m.data.and_then(|d| serde_json::from_value::<EventDto>(d).ok())
                    && self.events.len() < 4096 {
                        self.events.push_back(e);
                    }
                continue;
            }
            match (&m.id, m.error, m.result) {
                (Some(rid), err, res) if *rid == id => {
                    return match err {
                        Some(e) => Err(ClientError::Api(e)),
                        None => Ok(res.unwrap_or(Value::Null)),
                    };
                }
                // The server speaks first only to refuse us (access off, too many clients).
                (None, Some(e), _) => return Err(ClientError::Api(e)),
                _ => continue,
            }
        }
    }

    /// `call` and decode the result.
    pub async fn call_as<T: DeserializeOwned>(&mut self, req: &Request) -> Result<T, ClientError> {
        let v = self.call(req).await?;
        serde_json::from_value(v).map_err(|e| ClientError::Protocol(e.to_string()))
    }

    /// The next pushed event (after `events_subscribe`), waiting at most `timeout`.
    pub async fn next_event(&mut self, timeout: Duration) -> Result<Option<EventDto>, ClientError> {
        if let Some(e) = self.events.pop_front() {
            return Ok(Some(e));
        }
        match tokio::time::timeout(timeout, self.read_message()).await {
            Err(_) => Ok(None),
            Ok(Err(e)) => Err(e),
            Ok(Ok(m)) => {
                if m.event.is_some() {
                    Ok(m.data.and_then(|d| serde_json::from_value::<EventDto>(d).ok()))
                } else if let Some(e) = m.error {
                    Err(ClientError::Api(e))
                } else {
                    Ok(None)
                }
            }
        }
    }
}
