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

pub struct AppState {
    repo_root: PathBuf,
    lock: Mutex<()>,
}

impl AppState {
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
            lock: Mutex::new(()),
        }
    }

    fn open(&self) -> Result<Repository, Error> {
        Repository::open(&self.repo_root)
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/branches", get(list_branches))
        .route("/api/v1/branches/:name", get(get_branch))
        .route("/api/v1/missing", get(missing))
        .route("/api/v1/bundle", post(post_bundle))
        .with_state(state)
}

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
