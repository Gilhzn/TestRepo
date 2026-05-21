//! Real-time co-editing via WebSocket relay.
//!
//! For each named document the server keeps an append-only log of binary
//! updates plus the set of currently connected peers. Updates flow through:
//!
//!   client → server (binary frame)
//!     ↓ append to log
//!     ↓ broadcast to every other connected peer
//!
//! When a new peer connects, the server first replays the entire log so the
//! peer reaches eventual consistency, then attaches them to the broadcast
//! channel. The updates themselves are opaque bytes (typically Yjs v1
//! update format produced by `CrdtDoc::encode_update_since`) — the server
//! doesn't parse them, which keeps it agnostic to the wire format and
//! sidesteps the fact that `yrs::Doc` is not `Send`.
//!
//! This is intentionally simpler than full y-websocket: no awareness, no
//! state-vector negotiation. Compaction (collapsing the log into a single
//! merged update at quiescence) lands later.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};

/// Cap on the per-room update log. When exceeded the oldest entries are
/// dropped — new joiners see only the last N updates, which is fine for
/// CRDT clients that already have most state out of band. Tune with
/// `set_history_cap` per-room or leave at the conservative default.
pub const DEFAULT_HISTORY_CAP: usize = 10_000;

#[derive(Clone)]
pub struct DocRoom {
    pub history: Arc<Mutex<Vec<Vec<u8>>>>,
    pub history_cap: Arc<std::sync::atomic::AtomicUsize>,
    pub broadcast: broadcast::Sender<Vec<u8>>,
}

impl DocRoom {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(1024);
        Self {
            history: Arc::new(Mutex::new(Vec::new())),
            history_cap: Arc::new(std::sync::atomic::AtomicUsize::new(DEFAULT_HISTORY_CAP)),
            broadcast: tx,
        }
    }

    pub fn set_history_cap(&self, cap: usize) {
        self.history_cap
            .store(cap.max(1), std::sync::atomic::Ordering::Relaxed);
    }

    pub fn history_cap(&self) -> usize {
        self.history_cap.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Append an update, enforcing the cap by dropping the oldest entries.
    pub async fn push_update(&self, update: Vec<u8>) {
        let cap = self.history_cap();
        let mut log = self.history.lock().await;
        log.push(update);
        if log.len() > cap {
            let excess = log.len() - cap;
            log.drain(0..excess);
        }
    }

    pub async fn history_len(&self) -> usize {
        self.history.lock().await.len()
    }
}

impl Default for DocRoom {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
pub struct LiveState {
    rooms: Mutex<HashMap<String, DocRoom>>,
}

impl LiveState {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn room(&self, name: &str) -> DocRoom {
        let mut map = self.rooms.lock().await;
        map.entry(name.to_string()).or_insert_with(DocRoom::new).clone()
    }

    pub async fn open_doc_count(&self) -> usize {
        self.rooms.lock().await.len()
    }

    pub async fn subscriber_count(&self, name: &str) -> usize {
        let map = self.rooms.lock().await;
        map.get(name).map(|r| r.broadcast.receiver_count()).unwrap_or(0)
    }
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    Path(name): Path<String>,
    State(state): State<Arc<crate::AppState>>,
) -> impl IntoResponse {
    let live = state.live.clone();
    ws.on_upgrade(move |socket| handle_socket(socket, name, live))
}

async fn handle_socket(socket: WebSocket, name: String, live: Arc<LiveState>) {
    let room = live.room(&name).await;
    let mut rx = room.broadcast.subscribe();
    let (mut sender, mut receiver) = socket.split();

    // 1. Replay the existing history so the new peer is caught up.
    {
        let log = room.history.lock().await;
        for update in log.iter() {
            if sender.send(Message::Binary(update.clone())).await.is_err() {
                return;
            }
        }
    }

    // 2. Spawn a forwarder for inbound broadcasts.
    let forward_task = {
        let mut sender = sender;
        tokio::spawn(async move {
            while let Ok(update) = rx.recv().await {
                if sender.send(Message::Binary(update)).await.is_err() {
                    break;
                }
            }
            let _ = sender.close().await;
        })
    };

    // 3. Read inbound updates from this peer, append + broadcast.
    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            Message::Binary(bytes) => {
                room.push_update(bytes.clone()).await;
                let _ = room.broadcast.send(bytes);
            }
            Message::Text(_) => {
                // Ignore non-binary messages for now.
            }
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }

    forward_task.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn room_is_unique_per_name() {
        let live = LiveState::new();
        let a1 = live.room("file-a.txt").await;
        let a2 = live.room("file-a.txt").await;
        let b = live.room("file-b.txt").await;
        assert_eq!(live.open_doc_count().await, 2);
        // a1 and a2 should be the same broadcast channel.
        let mut rx = a2.broadcast.subscribe();
        a1.broadcast.send(vec![1, 2, 3]).unwrap();
        assert_eq!(rx.recv().await.unwrap(), vec![1, 2, 3]);
        // b is independent.
        let mut rx_b = b.broadcast.subscribe();
        assert!(rx_b.try_recv().is_err());
    }

    #[tokio::test]
    async fn subscriber_count_zero_initially() {
        let live = LiveState::new();
        let _ = live.room("x").await;
        assert_eq!(live.subscriber_count("x").await, 0);
    }

    #[tokio::test]
    async fn history_appends_in_order() {
        let live = LiveState::new();
        let room = live.room("file").await;
        room.push_update(vec![1]).await;
        room.push_update(vec![2]).await;
        room.push_update(vec![3]).await;
        let log = room.history.lock().await;
        assert_eq!(*log, vec![vec![1], vec![2], vec![3]]);
    }

    #[tokio::test]
    async fn history_cap_drops_oldest_entries() {
        let live = LiveState::new();
        let room = live.room("file").await;
        room.set_history_cap(3);
        for i in 0..10u8 {
            room.push_update(vec![i]).await;
        }
        let log = room.history.lock().await;
        // Last 3 only.
        assert_eq!(*log, vec![vec![7u8], vec![8u8], vec![9u8]]);
    }

    #[tokio::test]
    async fn default_history_cap_is_reasonable() {
        let live = LiveState::new();
        let room = live.room("any").await;
        assert!(room.history_cap() >= 1_000);
    }
}
