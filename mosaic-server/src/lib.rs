//! Mosaic HTTP sync server.
//!
//! Hosts one repository over a small REST API so two or more peers (humans,
//! agents) on different machines can converge on a shared history without a
//! third-party Git forge.
//!
//! Endpoints (all under `/api/v1`):
//!
//!   GET  /branches                         list branch names + frontier sizes
//!   GET  /branches/:name                   return that branch's frontier as JSON
//!   GET  /missing?branch=&have=<hex,hex>   bundle of changes the client lacks
//!   POST /bundle                           apply a raw bundle (octet-stream)
//!   GET  /health                           liveness probe
//!
//! There is intentionally no auth or TLS yet — that lands in M7. Network
//! deployment in production should sit behind a reverse proxy.

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use mosaic_core::error::Error;
use mosaic_core::m1::change::ChangeId;
use mosaic_core::m1_dag::refs::Frontier;
use mosaic_core::repo::Repository;
use mosaic_core::sync::{apply_bundle, build_bundle_for_branch, missing_changes_for, Bundle};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

pub mod auth;
pub mod ws;

pub struct AppState {
    repo_root: PathBuf,
    lock: Mutex<()>,
    policy: auth::Policy,
    pub live: Arc<ws::LiveState>,
}

impl AppState {
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        let repo_root = repo_root.into();
        let policy = auth::Policy::load(&repo_root).unwrap_or_default();
        Self {
            repo_root,
            lock: Mutex::new(()),
            policy,
            live: Arc::new(ws::LiveState::new()),
        }
    }

    pub fn with_policy(repo_root: impl Into<PathBuf>, policy: auth::Policy) -> Self {
        Self {
            repo_root: repo_root.into(),
            lock: Mutex::new(()),
            policy,
            live: Arc::new(ws::LiveState::new()),
        }
    }

    fn open(&self) -> Result<Repository, Error> {
        Repository::open(&self.repo_root)
    }

    pub fn policy(&self) -> &auth::Policy {
        &self.policy
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index_html))
        .route("/api/v1/health", get(health))
        .route("/api/v1/branches", get(list_branches))
        .route("/api/v1/branches/:name", get(get_branch))
        .route("/api/v1/changes", get(list_changes))
        .route("/api/v1/changes/:id", get(get_change))
        .route("/api/v1/missing", get(missing))
        .route("/api/v1/bundle", post(post_bundle))
        .route("/api/v1/live-stats", get(live_stats))
        .route("/ws/doc/:name", get(ws::ws_handler))
        .with_state(state)
}

#[derive(Serialize)]
pub struct LiveStats {
    pub open_docs: usize,
}

async fn live_stats(State(s): State<Arc<AppState>>) -> Json<LiveStats> {
    Json(LiveStats {
        open_docs: s.live.open_doc_count().await,
    })
}

#[derive(Serialize, Deserialize)]
pub struct ChangeSummary {
    pub id: String,
    pub author: String,
    pub intent: Option<String>,
    pub timestamp: u64,
    pub deps: Vec<String>,
    pub file_count: usize,
}

async fn list_changes(State(s): State<Arc<AppState>>) -> Result<Json<Vec<ChangeSummary>>, AppError> {
    let repo = s.open()?;
    let all = repo.all_change_ids()?;
    let ordered = repo.topo_order(&all);
    let mut out = Vec::new();
    for h in ordered {
        let change = repo.load_change(&ChangeId(h))?;
        out.push(ChangeSummary {
            id: h.to_hex(),
            author: change.author.display(),
            intent: change.intent,
            timestamp: change.ts.0,
            deps: change.deps.iter().map(|d| d.to_hex()).collect(),
            file_count: change.body.len(),
        });
    }
    Ok(Json(out))
}

async fn get_change(
    State(s): State<Arc<AppState>>,
    Path(id_hex): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let repo = s.open()?;
    let h = mosaic_core::Hash::from_hex(&id_hex)?;
    let change = repo.load_change(&ChangeId(h))?;
    let files: Vec<serde_json::Value> = change
        .body
        .iter()
        .map(|f| {
            serde_json::json!({
                "path": f.path,
                "kind": format!("{:?}", f.kind),
                "size": f.patch.len(),
            })
        })
        .collect();
    Ok(Json(serde_json::json!({
        "id": id_hex,
        "author": change.author.display(),
        "intent": change.intent,
        "timestamp": change.ts.0,
        "deps": change.deps.iter().map(|d| d.to_hex()).collect::<Vec<_>>(),
        "files": files,
    })))
}

async fn index_html() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        INDEX_HTML,
    )
}

const INDEX_HTML: &str = include_str!("index.html");

#[derive(Serialize, Deserialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub repo: String,
    pub changes: usize,
    pub branches: usize,
}

async fn health(State(s): State<Arc<AppState>>) -> Result<Json<HealthResponse>, AppError> {
    let repo = s.open()?;
    Ok(Json(HealthResponse {
        ok: true,
        repo: repo.root().display().to_string(),
        changes: repo.all_change_ids()?.len(),
        branches: repo.refs().list()?.len(),
    }))
}

#[derive(Serialize, Deserialize)]
pub struct BranchSummary {
    pub name: String,
    pub tips: Vec<String>,
}

async fn list_branches(State(s): State<Arc<AppState>>) -> Result<Json<Vec<BranchSummary>>, AppError> {
    let repo = s.open()?;
    let mut out = Vec::new();
    for name in repo.refs().list()? {
        let f = repo.refs().get(&name)?;
        out.push(BranchSummary {
            name,
            tips: f.0.iter().map(|h| h.to_hex()).collect(),
        });
    }
    Ok(Json(out))
}

async fn get_branch(
    State(s): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<BranchSummary>, AppError> {
    let repo = s.open()?;
    let f = repo.refs().get(&name)?;
    Ok(Json(BranchSummary {
        name,
        tips: f.0.iter().map(|h| h.to_hex()).collect(),
    }))
}

#[derive(Deserialize)]
struct MissingQuery {
    branch: String,
    #[serde(default)]
    have: String,
}

async fn missing(
    State(s): State<Arc<AppState>>,
    Query(q): Query<MissingQuery>,
) -> Result<impl IntoResponse, AppError> {
    let repo = s.open()?;
    let tips = repo.refs().get(&q.branch)?;
    let receiver_frontier = parse_have(&q.have)?;
    let needed = missing_changes_for(&repo, &receiver_frontier, &tips);
    let bundle = build_bundle_for_branch(&repo, &q.branch, &needed)?;
    let bytes = bundle.encode()?;
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, "application/octet-stream"),
        ],
        bytes,
    ))
}

#[derive(Serialize, Deserialize)]
pub struct ApplyResponse {
    pub applied: Vec<String>,
    pub skipped: Vec<String>,
}

async fn post_bundle(
    State(s): State<Arc<AppState>>,
    body: Bytes,
) -> Result<Json<ApplyResponse>, AppError> {
    let bundle = Bundle::decode(&body)?;
    s.policy
        .authorize_bundle(&bundle)
        .map_err(AppError::Forbidden)?;
    let _guard = s.lock.lock().await;
    let mut repo = s.open()?;
    let report = apply_bundle(&mut repo, &bundle)?;
    Ok(Json(ApplyResponse {
        applied: report.applied.iter().map(ChangeId::to_hex).collect(),
        skipped: report.skipped.iter().map(ChangeId::to_hex).collect(),
    }))
}

fn parse_have(s: &str) -> Result<Frontier, AppError> {
    let mut set = BTreeSet::new();
    if s.trim().is_empty() {
        return Ok(Frontier(set));
    }
    for part in s.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        let h = mosaic_core::Hash::from_hex(p).map_err(AppError::from)?;
        set.insert(h);
    }
    Ok(Frontier(set))
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Core(#[from] Error),
    #[error("forbidden: {0}")]
    Forbidden(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let (status, msg) = match &self {
            AppError::Core(Error::RefNotFound(name)) => {
                (StatusCode::NOT_FOUND, format!("ref not found: {name}"))
            }
            AppError::Core(Error::UnknownParent(s)) => (
                StatusCode::CONFLICT,
                format!("unknown parent change: {s}"),
            ),
            AppError::Core(Error::BadSignature) => (
                StatusCode::FORBIDDEN,
                "signature verification failed".into(),
            ),
            AppError::Forbidden(s) => (StatusCode::FORBIDDEN, s.clone()),
            other => (StatusCode::INTERNAL_SERVER_ERROR, format!("{other}")),
        };
        let body = serde_json::json!({ "error": msg });
        (status, Json(body)).into_response()
    }
}

/// Start the server on `addr` against the repository at `repo_root`. Returns
/// the bound socket address (useful for tests where you bind port 0) and a
/// handle the caller can drop or await.
pub async fn serve(
    repo_root: impl Into<PathBuf>,
    addr: std::net::SocketAddr,
) -> Result<(std::net::SocketAddr, tokio::task::JoinHandle<()>), std::io::Error> {
    let state = Arc::new(AppState::new(repo_root));
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((bound, handle))
}

pub fn repo_root_default() -> PathBuf {
    PathBuf::from(".")
}

pub fn ensure_repo(path: &StdPath) -> Result<(), Error> {
    Repository::open(path).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mosaic_core::m1::change::{ChangeBuilder, FileChange, FileKind};
    use mosaic_core::m1::identity::Identity;
    use mosaic_core::m1::signing::SigningKey;
    use tempfile::TempDir;

    fn human() -> (Identity, SigningKey) {
        (
            Identity::human("dev@example.com", Some("Dev".into())).unwrap(),
            SigningKey::generate(),
        )
    }

    fn one_commit(repo: &mut Repository, idn: &Identity, key: &SigningKey, intent: &str) -> ChangeId {
        let change = ChangeBuilder::new(idn.clone(), key.clone())
            .intent(intent)
            .file(FileChange {
                path: format!("{intent}.txt"),
                kind: FileKind::Text,
                patch: intent.as_bytes().to_vec(),
                conflicts: Vec::new(),
            })
            .build()
            .unwrap();
        let id = repo.commit(change).unwrap();
        repo.advance_branch("main", id).unwrap();
        id
    }

    #[tokio::test]
    async fn health_endpoint_reports_repo_state() {
        let dir = TempDir::new().unwrap();
        let _ = Repository::init(dir.path()).unwrap();
        let (idn, key) = human();
        {
            let mut repo = Repository::open(dir.path()).unwrap();
            one_commit(&mut repo, &idn, &key, "first");
        }
        let (addr, handle) =
            serve(dir.path(), "127.0.0.1:0".parse().unwrap()).await.unwrap();

        let url = format!("http://{addr}/api/v1/health");
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: HealthResponse = resp.json().await.unwrap();
        assert!(body.ok);
        assert_eq!(body.changes, 1);
        assert_eq!(body.branches, 1);

        handle.abort();
    }

    #[tokio::test]
    async fn push_and_pull_round_trip() {
        let server_dir = TempDir::new().unwrap();
        let client_dir = TempDir::new().unwrap();
        let _ = Repository::init(server_dir.path()).unwrap();
        let _ = Repository::init(client_dir.path()).unwrap();

        let (idn, key) = human();
        let local_id = {
            let mut repo = Repository::open(client_dir.path()).unwrap();
            one_commit(&mut repo, &idn, &key, "first")
        };

        let (addr, handle) =
            serve(server_dir.path(), "127.0.0.1:0".parse().unwrap()).await.unwrap();
        let base_url = format!("http://{addr}");

        let bundle = {
            let repo = Repository::open(client_dir.path()).unwrap();
            let all: Vec<ChangeId> = repo
                .all_change_ids()
                .unwrap()
                .into_iter()
                .map(ChangeId)
                .collect();
            build_bundle_for_branch(&repo, "main", &all).unwrap()
        };

        let resp = reqwest::Client::new()
            .post(format!("{base_url}/api/v1/bundle"))
            .body(bundle.encode().unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let applied: ApplyResponse = resp.json().await.unwrap();
        assert_eq!(applied.applied.len(), 1);

        let server_branches: Vec<BranchSummary> = reqwest::get(format!("{base_url}/api/v1/branches"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(server_branches.len(), 1);

        let third_dir = TempDir::new().unwrap();
        let _ = Repository::init(third_dir.path()).unwrap();
        let missing_url = format!(
            "{base_url}/api/v1/missing?branch={branch}&have=",
            branch = server_branches[0].name
        );
        let missing_bytes = reqwest::get(&missing_url).await.unwrap().bytes().await.unwrap();
        let bundle = Bundle::decode(&missing_bytes).unwrap();
        assert_eq!(bundle.changes.len(), 1);
        assert_eq!(bundle.changes[0].id(), local_id);

        handle.abort();
    }

    #[tokio::test]
    async fn allowlist_rejects_unknown_signer_over_http() {
        let server_dir = TempDir::new().unwrap();
        let _ = Repository::init(server_dir.path()).unwrap();
        let trusted_key = SigningKey::generate();
        let attacker_key = SigningKey::generate();

        // Server policy: only the trusted key is allowed.
        let policy = auth::Policy::allowlist([trusted_key.verifying_key().to_bytes()]);

        // Build a bundle signed by the ATTACKER key (not on the allowlist).
        let bundle = {
            let bundle_dir = TempDir::new().unwrap();
            let mut repo = Repository::init(bundle_dir.path()).unwrap();
            let idn = Identity::human("attacker@example.com", None).unwrap();
            let cid = one_commit(&mut repo, &idn, &attacker_key, "evil");
            mosaic_core::sync::build_bundle(&repo, &[cid]).unwrap()
        };

        let state = Arc::new(AppState::with_policy(server_dir.path(), policy));
        let app = router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let body = bundle.encode().unwrap();
        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/api/v1/bundle"))
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Bundle signed by the trusted key goes through.
        let trusted_bundle = {
            let bundle_dir = TempDir::new().unwrap();
            let mut repo = Repository::init(bundle_dir.path()).unwrap();
            let idn = Identity::human("alice@example.com", None).unwrap();
            let cid = one_commit(&mut repo, &idn, &trusted_key, "ok");
            mosaic_core::sync::build_bundle(&repo, &[cid]).unwrap()
        };
        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/api/v1/bundle"))
            .body(trusted_bundle.encode().unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        handle.abort();
    }

    #[tokio::test]
    async fn websocket_broadcasts_updates_between_peers() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message as WsMsg;

        let dir = TempDir::new().unwrap();
        let _ = Repository::init(dir.path()).unwrap();
        let (addr, handle) =
            serve(dir.path(), "127.0.0.1:0".parse().unwrap()).await.unwrap();

        let url = format!("ws://{addr}/ws/doc/payments.rs");

        let (mut alice, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        // Give Alice's subscriber time to register.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (mut bob, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Alice sends an update; Bob should receive it.
        alice
            .send(WsMsg::Binary(b"hello-from-alice".to_vec()))
            .await
            .unwrap();

        let received = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            async {
                while let Some(Ok(msg)) = bob.next().await {
                    if let WsMsg::Binary(b) = msg {
                        return b;
                    }
                }
                panic!("bob stream ended without binary message");
            },
        )
        .await
        .unwrap();
        assert_eq!(received, b"hello-from-alice");

        // A new peer Carol connects and immediately receives the replay
        // (alice's earlier update).
        let (mut carol, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let replay = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            async {
                while let Some(Ok(msg)) = carol.next().await {
                    if let WsMsg::Binary(b) = msg {
                        return b;
                    }
                }
                panic!("carol stream ended without binary message");
            },
        )
        .await
        .unwrap();
        assert_eq!(replay, b"hello-from-alice");

        let _ = alice.close(None).await;
        let _ = bob.close(None).await;
        let _ = carol.close(None).await;
        handle.abort();
    }

    #[tokio::test]
    async fn websocket_separate_docs_dont_interfere() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message as WsMsg;

        let dir = TempDir::new().unwrap();
        let _ = Repository::init(dir.path()).unwrap();
        let (addr, handle) =
            serve(dir.path(), "127.0.0.1:0".parse().unwrap()).await.unwrap();

        let url_a = format!("ws://{addr}/ws/doc/a");
        let url_b = format!("ws://{addr}/ws/doc/b");

        let (mut alice, _) = tokio_tungstenite::connect_async(&url_a).await.unwrap();
        let (mut bob, _) = tokio_tungstenite::connect_async(&url_b).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        alice.send(WsMsg::Binary(b"alice-only".to_vec())).await.unwrap();

        // Bob should NOT receive anything in 200ms.
        let got = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            bob.next(),
        )
        .await;
        assert!(got.is_err(), "bob received unexpected cross-doc traffic");

        let _ = alice.close(None).await;
        let _ = bob.close(None).await;
        handle.abort();
    }

    #[tokio::test]
    async fn missing_only_returns_delta() {
        let server_dir = TempDir::new().unwrap();
        let _ = Repository::init(server_dir.path()).unwrap();
        let (idn, key) = human();
        let (a, b) = {
            let mut repo = Repository::open(server_dir.path()).unwrap();
            let a = one_commit(&mut repo, &idn, &key, "a");
            let b = one_commit(&mut repo, &idn, &key, "b");
            (a, b)
        };

        let (addr, handle) =
            serve(server_dir.path(), "127.0.0.1:0".parse().unwrap()).await.unwrap();
        let base_url = format!("http://{addr}");

        let url = format!("{base_url}/api/v1/missing?branch=main&have={}", a.to_hex());
        let bytes = reqwest::get(&url).await.unwrap().bytes().await.unwrap();
        let bundle = Bundle::decode(&bytes).unwrap();
        assert_eq!(bundle.changes.len(), 1);
        assert_eq!(bundle.changes[0].id(), b);

        handle.abort();
    }
}
