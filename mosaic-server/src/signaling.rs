//! WebRTC signaling channel.
//!
//! Peers exchange SDP offers / answers / ICE candidates so they can open
//! direct WebRTC data channels without the server staying in the data
//! path. Unlike the awareness channel (broadcast-only), signaling supports
//! **directed** messages so a peer can target a specific other peer:
//!
//! Client → server JSON envelope:
//!   { "type": "offer", "to": <peer_id>, "sdp": "..." }
//!
//! If `to` is present the server delivers ONLY to that peer; otherwise the
//! message is broadcast (same shape as awareness). Either way, the server
//! authoritatively tags the envelope with `"from": <sender peer id>` so
//! recipients don't have to trust the sender's self-id.
//!
//! The server never inspects offer/answer bodies — it's a relay, not a TURN
//! server. Once the offer/answer/ICE exchange completes, peers talk
//! directly (or fall back to the WebSocket relay if NAT traversal fails).

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

#[derive(Clone)]
struct PeerSink {
    tx: mpsc::UnboundedSender<String>,
}

#[derive(Default)]
struct SignalingRoom {
    peers: Mutex<HashMap<u64, PeerSink>>,
}

#[derive(Default)]
pub struct SignalingState {
    rooms: Mutex<HashMap<String, Arc<SignalingRoom>>>,
    next_peer_id: AtomicU64,
}

impl SignalingState {
    pub fn new() -> Self {
        Self::default()
    }

    fn next_peer_id(&self) -> u64 {
        self.next_peer_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    async fn room(&self, name: &str) -> Arc<SignalingRoom> {
        let mut map = self.rooms.lock().await;
        map.entry(name.to_string())
            .or_insert_with(|| Arc::new(SignalingRoom::default()))
            .clone()
    }

    pub async fn room_count(&self) -> usize {
        self.rooms.lock().await.len()
    }

    pub async fn peer_count(&self, name: &str) -> usize {
        let map = self.rooms.lock().await;
        match map.get(name) {
            Some(room) => room.peers.lock().await.len(),
            None => 0,
        }
    }
}

pub async fn signaling_handler(
    ws: WebSocketUpgrade,
    Path(name): Path<String>,
    State(state): State<Arc<crate::AppState>>,
) -> impl IntoResponse {
    let signaling = state.signaling.clone();
    ws.on_upgrade(move |socket| handle_signaling(socket, name, signaling))
}

async fn handle_signaling(socket: WebSocket, name: String, state: Arc<SignalingState>) {
    let room = state.room(&name).await;
    let peer_id = state.next_peer_id();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    {
        let mut peers = room.peers.lock().await;
        peers.insert(peer_id, PeerSink { tx: tx.clone() });
    }

    let (mut sender, mut receiver) = socket.split();

    // Tell the joining peer its server-assigned id + the current roster.
    let roster: Vec<u64> = {
        let peers = room.peers.lock().await;
        peers.keys().copied().collect()
    };
    let hello = json!({
        "type": "hello",
        "peer": peer_id,
        "roster": roster,
    })
    .to_string();
    if sender.send(Message::Text(hello)).await.is_err() {
        return;
    }

    // Broadcast a join event to everyone *except* the joiner.
    {
        let peers = room.peers.lock().await;
        let join_msg = json!({ "type": "join", "peer": peer_id }).to_string();
        for (pid, peer) in peers.iter() {
            if *pid != peer_id {
                let _ = peer.tx.send(join_msg.clone());
            }
        }
    }

    // Forward outbound queue to the socket.
    let forward_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sender.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
        let _ = sender.close().await;
    });

    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            Message::Text(text) => {
                let parsed: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let mut envelope = parsed.as_object().cloned().unwrap_or_default();
                envelope.insert("from".into(), json!(peer_id));
                let to = envelope.get("to").and_then(Value::as_u64);
                let serialized = serde_json::Value::Object(envelope).to_string();

                let peers = room.peers.lock().await;
                match to {
                    Some(target) => {
                        if let Some(peer) = peers.get(&target) {
                            let _ = peer.tx.send(serialized);
                        }
                    }
                    None => {
                        for (pid, peer) in peers.iter() {
                            if *pid != peer_id {
                                let _ = peer.tx.send(serialized.clone());
                            }
                        }
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    // Drop peer, broadcast leave.
    {
        let mut peers = room.peers.lock().await;
        peers.remove(&peer_id);
        let leave_msg = json!({ "type": "leave", "peer": peer_id }).to_string();
        for peer in peers.values() {
            let _ = peer.tx.send(leave_msg.clone());
        }
    }
    forward_task.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fresh_state_has_no_rooms() {
        let s = SignalingState::new();
        assert_eq!(s.room_count().await, 0);
    }

    #[tokio::test]
    async fn peer_ids_are_monotonic() {
        let s = SignalingState::new();
        let a = s.next_peer_id();
        let b = s.next_peer_id();
        let c = s.next_peer_id();
        assert!(b > a);
        assert!(c > b);
    }

    #[tokio::test]
    async fn rooms_are_distinct_per_name() {
        let s = SignalingState::new();
        let _ = s.room("call-a").await;
        let _ = s.room("call-b").await;
        let _ = s.room("call-a").await;
        assert_eq!(s.room_count().await, 2);
    }
}
