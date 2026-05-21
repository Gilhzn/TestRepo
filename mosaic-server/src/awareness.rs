//! Presence / awareness channel.
//!
//! Sibling to `ws::DocRoom`. Same broadcast pattern but for short-lived
//! presence messages (cursor positions, user color, peer joined / left).
//! Clients send JSON text frames; the server doesn't parse them — it just
//! tags each broadcast with a server-assigned peer id so receivers can
//! distinguish senders without trusting the senders' self-reported ids.
//!
//! Unlike `ws::DocRoom`, awareness messages are NOT replayed to new
//! joiners. Presence is ephemeral; when a peer disconnects the server
//! emits a `{"type":"leave","peer":<id>}` notification on its behalf so
//! other clients can drop the entry.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};

#[derive(Clone)]
pub struct PresenceRoom {
    pub broadcast: broadcast::Sender<String>,
}

impl PresenceRoom {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(1024);
        Self { broadcast: tx }
    }
}

impl Default for PresenceRoom {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
pub struct PresenceState {
    rooms: Mutex<HashMap<String, PresenceRoom>>,
    next_peer_id: AtomicU64,
}

impl PresenceState {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn room(&self, name: &str) -> PresenceRoom {
        let mut map = self.rooms.lock().await;
        map.entry(name.to_string()).or_insert_with(PresenceRoom::new).clone()
    }

    pub fn next_peer_id(&self) -> u64 {
        self.next_peer_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub async fn room_count(&self) -> usize {
        self.rooms.lock().await.len()
    }

    pub async fn subscriber_count(&self, name: &str) -> usize {
        let map = self.rooms.lock().await;
        map.get(name).map(|r| r.broadcast.receiver_count()).unwrap_or(0)
    }
}

pub async fn awareness_handler(
    ws: WebSocketUpgrade,
    Path(name): Path<String>,
    State(state): State<Arc<crate::AppState>>,
) -> impl IntoResponse {
    let presence = state.presence.clone();
    ws.on_upgrade(move |socket| handle_presence(socket, name, presence))
}

async fn handle_presence(socket: WebSocket, name: String, state: Arc<PresenceState>) {
    let room = state.room(&name).await;
    let peer_id = state.next_peer_id();
    let mut rx = room.broadcast.subscribe();
    let (mut sender, mut receiver) = socket.split();

    // Tell the joining peer its server-assigned id.
    let hello = json!({ "type": "hello", "peer": peer_id }).to_string();
    if sender.send(Message::Text(hello)).await.is_err() {
        return;
    }

    let tx_clone = room.broadcast.clone();
    let join_msg = json!({ "type": "join", "peer": peer_id }).to_string();
    let _ = tx_clone.send(join_msg);

    let forward_task = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            if sender.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
        let _ = sender.close().await;
    });

    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            Message::Text(text) => {
                // Re-emit tagged with the server's authoritative peer id so
                // receivers don't have to trust the sender's self-id.
                let envelope = match serde_json::from_str::<serde_json::Value>(&text) {
                    Ok(mut v) => {
                        if let Some(obj) = v.as_object_mut() {
                            obj.insert("peer".into(), json!(peer_id));
                        }
                        v.to_string()
                    }
                    Err(_) => json!({ "type": "raw", "peer": peer_id, "data": text }).to_string(),
                };
                let _ = room.broadcast.send(envelope);
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    let leave_msg = json!({ "type": "leave", "peer": peer_id }).to_string();
    let _ = room.broadcast.send(leave_msg);
    forward_task.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn peer_ids_are_monotonic() {
        let state = PresenceState::new();
        let a = state.next_peer_id();
        let b = state.next_peer_id();
        let c = state.next_peer_id();
        assert!(b > a);
        assert!(c > b);
    }

    #[tokio::test]
    async fn rooms_are_unique_per_name() {
        let state = PresenceState::new();
        let a = state.room("file-a").await;
        let b = state.room("file-b").await;
        assert_eq!(state.room_count().await, 2);
        let mut rx_a = a.broadcast.subscribe();
        let mut rx_b = b.broadcast.subscribe();
        a.broadcast.send("hello-a".into()).unwrap();
        assert_eq!(rx_a.recv().await.unwrap(), "hello-a");
        assert!(rx_b.try_recv().is_err());
    }
}
