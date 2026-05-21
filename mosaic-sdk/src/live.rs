//! Real-time co-editing client.
//!
//! Enabled via the `live` feature. Connects to a `mosaic-serve` instance's
//! `GET /ws/doc/:name` endpoint and yields a duplex stream of binary
//! updates. The protocol is intentionally a thin relay: peers exchange
//! opaque byte frames, each of which is a Yjs (or any other CRDT) update.
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use mosaic_sdk::live::LiveSession;
//! let mut session = LiveSession::connect("ws://localhost:7700/ws/doc/payments.rs").await?;
//! session.send(b"hello").await?;
//! while let Some(update) = session.recv().await {
//!     println!("got {} bytes", update.len());
//! }
//! # Ok(())
//! # }
//! ```

use futures_util::{SinkExt, StreamExt};
use std::fmt;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Stream = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct LiveSession {
    stream: Stream,
}

#[derive(Debug)]
pub enum LiveError {
    Connect(String),
    Send(String),
    Closed,
}

impl fmt::Display for LiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(s) => write!(f, "connect: {s}"),
            Self::Send(s) => write!(f, "send: {s}"),
            Self::Closed => write!(f, "session closed"),
        }
    }
}

impl std::error::Error for LiveError {}

impl From<WsError> for LiveError {
    fn from(e: WsError) -> Self {
        Self::Send(e.to_string())
    }
}

impl LiveSession {
    pub async fn connect(url: &str) -> Result<Self, LiveError> {
        let (stream, _) = connect_async(url)
            .await
            .map_err(|e| LiveError::Connect(e.to_string()))?;
        Ok(Self { stream })
    }

    /// Send a binary update to the room. All other connected peers receive it.
    pub async fn send(&mut self, update: &[u8]) -> Result<(), LiveError> {
        self.stream
            .send(Message::Binary(update.to_vec()))
            .await
            .map_err(LiveError::from)?;
        Ok(())
    }

    /// Wait for the next binary update from any peer (including replay of
    /// the room's history when first connecting). Returns `None` when the
    /// connection closes.
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        while let Some(msg) = self.stream.next().await {
            match msg.ok()? {
                Message::Binary(b) => return Some(b),
                Message::Close(_) => return None,
                _ => continue,
            }
        }
        None
    }

    pub async fn close(mut self) -> Result<(), LiveError> {
        let _ = self.stream.close(None).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn round_trip_through_relay() {
        use tokio::net::TcpListener;
        use futures_util::stream::StreamExt;
        // Tiny in-process echo relay using tokio-tungstenite directly.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let history: Arc<tokio::sync::Mutex<Vec<Vec<u8>>>> = Arc::new(Default::default());
        let (tx, _) = tokio::sync::broadcast::channel::<Vec<u8>>(16);
        let tx_clone = tx.clone();
        let hist_clone = history.clone();
        tokio::spawn(async move {
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                let ws = tokio_tungstenite::accept_async(sock).await.unwrap();
                let (mut wtx, mut wrx) = ws.split();
                let mut sub = tx_clone.subscribe();
                let hist_for_reader = hist_clone.clone();
                let tx_for_reader = tx_clone.clone();
                // Replay history once on connect.
                {
                    let h = hist_clone.lock().await;
                    for u in h.iter() {
                        if wtx.send(Message::Binary(u.clone())).await.is_err() {
                            return;
                        }
                    }
                }
                tokio::spawn(async move {
                    while let Ok(b) = sub.recv().await {
                        if wtx.send(Message::Binary(b)).await.is_err() {
                            break;
                        }
                    }
                });
                tokio::spawn(async move {
                    while let Some(Ok(Message::Binary(b))) = wrx.next().await {
                        hist_for_reader.lock().await.push(b.clone());
                        let _ = tx_for_reader.send(b);
                    }
                });
            }
        });

        let mut alice = LiveSession::connect(&format!("ws://{addr}/")).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mut bob = LiveSession::connect(&format!("ws://{addr}/")).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        alice.send(b"yo").await.unwrap();
        let got = tokio::time::timeout(std::time::Duration::from_secs(2), bob.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got, b"yo");
    }
}
