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
use mosaic_core::issues::{Issue, IssueEvent, IssueEventBuilder, IssueEventKind, IssueStatus};
use mosaic_core::m1::change::ChangeId;
use mosaic_core::m1_dag::refs::Frontier;
use mosaic_core::repo::Repository;
use mosaic_core::review::{Approval, ApprovalBuilder, Comment, CommentAnchor, CommentBuilder, Verdict};
use mosaic_core::sync::{apply_bundle, build_bundle_for_branch, missing_changes_for, Bundle};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

pub mod auth;
pub mod awareness;
pub mod protection;
pub mod signaling;
pub mod webhooks;
pub mod ws;

pub struct AppState {
    repo_root: PathBuf,
    lock: Mutex<()>,
    policy: auth::Policy,
    pub rules: protection::ProtectionRules,
    pub live: Arc<ws::LiveState>,
    pub presence: Arc<awareness::PresenceState>,
    pub signaling: Arc<signaling::SignalingState>,
    /// Broadcast channel for server-sent push events.
    pub events: tokio::sync::broadcast::Sender<String>,
}

impl AppState {
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        let repo_root = repo_root.into();
        let policy = auth::Policy::load(&repo_root).unwrap_or_default();
        let rules = protection::ProtectionRules::load(&repo_root).unwrap_or_default();
        let (events, _) = tokio::sync::broadcast::channel(256);
        Self {
            repo_root,
            lock: Mutex::new(()),
            policy,
            rules,
            live: Arc::new(ws::LiveState::new()),
            presence: Arc::new(awareness::PresenceState::new()),
            signaling: Arc::new(signaling::SignalingState::new()),
            events,
        }
    }

    pub fn with_policy(repo_root: impl Into<PathBuf>, policy: auth::Policy) -> Self {
        let repo_root = repo_root.into();
        let rules = protection::ProtectionRules::load(&repo_root).unwrap_or_default();
        let (events, _) = tokio::sync::broadcast::channel(256);
        Self {
            repo_root,
            lock: Mutex::new(()),
            policy,
            rules,
            live: Arc::new(ws::LiveState::new()),
            presence: Arc::new(awareness::PresenceState::new()),
            signaling: Arc::new(signaling::SignalingState::new()),
            events,
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
        .route("/", get(landing_html))
        .route("/dashboard", get(index_html))
        .route("/api/v1/health", get(health))
        .route("/api/v1/branches", get(list_branches))
        .route("/api/v1/branches/:name", get(get_branch))
        .route("/api/v1/changes", get(list_changes))
        .route("/api/v1/changes/:id", get(get_change))
        .route("/api/v1/changes/:id/diff", get(get_change_diff))
        .route("/api/v1/graph", get(branch_graph))
        .route("/api/v1/missing", get(missing))
        .route("/api/v1/bundle", post(post_bundle))
        .route("/api/v1/changes/:id/comments", get(list_comments).post(post_comment))
        .route("/api/v1/changes/:id/comments/local", post(post_comment_local))
        .route("/api/v1/changes/:id/approvals", get(list_approvals).post(post_approval))
        .route("/api/v1/changes/:id/approvals/local", post(post_approval_local))
        .route("/api/v1/issues", get(list_issues).post(post_issue))
        .route("/api/v1/issues/local", post(post_issue_local))
        .route("/api/v1/issues/:number", get(get_issue))
        .route("/api/v1/issues/:number/events", post(post_issue_event))
        .route("/api/v1/issues/:number/events/local", post(post_issue_event_local))
        .route("/api/v1/live-stats", get(live_stats))
        .route("/api/v1/events", get(events_sse))
        .route("/ws/doc/:name", get(ws::ws_handler))
        .route("/ws/awareness/:name", get(awareness::awareness_handler))
        .route("/ws/signal/:name", get(signaling::signaling_handler))
        .route("/changes/:id", get(change_detail_html))
        .route("/issues", get(issues_html))
        .with_state(state)
}

#[derive(Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    pub kind: String,
    pub status: String,
    pub hunks: Vec<DiffHunk>,
}

#[derive(Serialize, Deserialize)]
pub struct DiffHunk {
    pub tag: String,
    pub line: String,
}

async fn get_change_diff(
    State(s): State<Arc<AppState>>,
    Path(id_hex): Path<String>,
) -> Result<Json<Vec<FileDiff>>, AppError> {
    let repo = s.open()?;
    let h = mosaic_core::Hash::from_hex(&id_hex)?;
    let change = repo.load_change(&ChangeId(h))?;

    let mut parent_files: std::collections::HashMap<String, Vec<u8>> = std::collections::HashMap::new();
    for parent in &change.deps {
        if let Ok(p_change) = repo.load_change(parent) {
            for file in &p_change.body {
                parent_files
                    .entry(file.path.clone())
                    .or_insert_with(|| file.patch.clone());
            }
        }
    }

    let mut out = Vec::new();
    for file in &change.body {
        let before_bytes = parent_files.get(&file.path).cloned().unwrap_or_default();
        let after_bytes = file.patch.clone();
        let status = if before_bytes.is_empty() {
            "added".to_string()
        } else if after_bytes.is_empty() {
            "removed".to_string()
        } else if before_bytes == after_bytes {
            "unchanged".to_string()
        } else {
            "modified".to_string()
        };

        let before = String::from_utf8_lossy(&before_bytes).into_owned();
        let after = String::from_utf8_lossy(&after_bytes).into_owned();
        let hunks = text_diff_hunks(&before, &after);

        out.push(FileDiff {
            path: file.path.clone(),
            kind: format!("{:?}", file.kind),
            status,
            hunks,
        });
    }
    Ok(Json(out))
}

fn text_diff_hunks(before: &str, after: &str) -> Vec<DiffHunk> {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(before, after);
    let mut out = Vec::new();
    for change in diff.iter_all_changes() {
        let tag = match change.tag() {
            ChangeTag::Equal => "equal",
            ChangeTag::Delete => "delete",
            ChangeTag::Insert => "insert",
        };
        out.push(DiffHunk {
            tag: tag.into(),
            line: change.value().trim_end_matches('\n').to_string(),
        });
    }
    out
}

async fn change_detail_html(Path(_id): Path<String>) -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        CHANGE_HTML,
    )
}

const CHANGE_HTML: &str = include_str!("change.html");

async fn issues_html() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        ISSUES_HTML,
    )
}

const ISSUES_HTML: &str = include_str!("issues.html");

#[derive(Serialize, Deserialize)]
pub struct GraphNode {
    pub id: String,
    pub author: String,
    pub intent: Option<String>,
    pub deps: Vec<String>,
    pub branches: Vec<String>,
    pub lane: usize,
    pub depth: usize,
}

#[derive(Serialize, Deserialize)]
pub struct BranchGraph {
    pub nodes: Vec<GraphNode>,
    pub lane_count: usize,
}

async fn branch_graph(State(s): State<Arc<AppState>>) -> Result<Json<BranchGraph>, AppError> {
    let repo = s.open()?;
    let all = repo.all_change_ids()?;
    let ordered = repo.topo_order(&all);

    let mut branch_tips: std::collections::HashMap<mosaic_core::Hash, Vec<String>> =
        std::collections::HashMap::new();
    for name in repo.refs().list()? {
        let f = repo.refs().get(&name)?;
        for tip in &f.0 {
            branch_tips
                .entry(*tip)
                .or_default()
                .push(name.clone());
        }
    }

    // Compute depth (longest path from a root) per change.
    let mut depth: std::collections::HashMap<mosaic_core::Hash, usize> =
        std::collections::HashMap::new();
    for h in &ordered {
        let parents = repo.index().parents_of(h).unwrap_or(&[]);
        let d = parents
            .iter()
            .map(|p| depth.get(p).copied().unwrap_or(0) + 1)
            .max()
            .unwrap_or(0);
        depth.insert(*h, d);
    }

    // Lane assignment: greedy left-most fit.
    // For each change in topo order, prefer the lane of its first parent if
    // free at this row, else first free lane, else new lane.
    let mut lanes: Vec<Option<mosaic_core::Hash>> = Vec::new();
    let mut lane_of: std::collections::HashMap<mosaic_core::Hash, usize> =
        std::collections::HashMap::new();
    let mut nodes = Vec::new();

    for h in &ordered {
        let parents = repo.index().parents_of(h).unwrap_or(&[]).to_vec();
        let preferred = parents.first().and_then(|p| lane_of.get(p).copied());
        let lane = match preferred {
            Some(l) => {
                lanes[l] = Some(*h);
                l
            }
            None => {
                let free = lanes.iter().position(|s| s.is_none());
                match free {
                    Some(idx) => {
                        lanes[idx] = Some(*h);
                        idx
                    }
                    None => {
                        lanes.push(Some(*h));
                        lanes.len() - 1
                    }
                }
            }
        };
        lane_of.insert(*h, lane);

        // Free the lanes of parents that this is the *last* child of.
        for p in &parents {
            let p_lane = lane_of.get(p).copied();
            let p_has_more_children = repo
                .index()
                .children_of(p)
                .map(|cs| {
                    cs.iter().any(|c| !lane_of.contains_key(c))
                })
                .unwrap_or(false);
            if !p_has_more_children {
                if let Some(pl) = p_lane {
                    if pl != lane && lanes.get(pl).and_then(|s| *s) == Some(*p) {
                        lanes[pl] = None;
                    }
                }
            }
        }

        let change = repo.load_change(&ChangeId(*h))?;
        let mut branches = branch_tips.get(h).cloned().unwrap_or_default();
        branches.sort();
        nodes.push(GraphNode {
            id: h.to_hex(),
            author: change.author.display(),
            intent: change.intent,
            deps: change.deps.iter().map(|d| d.to_hex()).collect(),
            branches,
            lane,
            depth: depth.get(h).copied().unwrap_or(0),
        });
    }

    Ok(Json(BranchGraph {
        lane_count: lanes.len(),
        nodes,
    }))
}

#[derive(Serialize, Deserialize)]
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

/// Server-sent event stream of push events. Clients `GET /api/v1/events`
/// and receive `data: {json}\n\n` frames whenever a push lands — the
/// long-lived complement to the pull-based `mos watch poll`.
async fn events_sse(
    State(s): State<Arc<AppState>>,
) -> axum::response::Sse<impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>>
{
    use axum::response::sse::{Event, KeepAlive, Sse};
    use futures_util::StreamExt;
    let rx = s.events.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|msg| async move {
        match msg {
            Ok(json) => Some(Ok(Event::default().data(json))),
            Err(_) => None, // lagged; skip
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn landing_html() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        LANDING_HTML,
    )
}

const LANDING_HTML: &str = include_str!("landing.html");

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
    let branch_advances = bundle.branch_advances.clone();
    let _guard = s.lock.lock().await;
    let mut repo = s.open()?;
    let report = apply_bundle(&mut repo, &bundle)?;
    let applied: Vec<String> = report.applied.iter().map(ChangeId::to_hex).collect();
    let skipped: Vec<String> = report.skipped.iter().map(ChangeId::to_hex).collect();

    // Fire webhooks for each branch_advance. Best-effort, non-blocking.
    let webhook_config =
        webhooks::WebhookConfig::load(&s.repo_root).unwrap_or_default();
    for (branch, frontier) in &branch_advances {
        let event = webhooks::Event::Push {
            branch: branch.clone(),
            tips: frontier.0.iter().map(|h| h.to_hex()).collect(),
            applied: applied.clone(),
            skipped: skipped.clone(),
        };
        webhooks::fire(&webhook_config, &event);
        // Also publish to the SSE event stream (best-effort).
        let sse_json = serde_json::json!({
            "type": "push",
            "branch": branch,
            "tips": frontier.0.iter().map(|h| h.to_hex()).collect::<Vec<_>>(),
            "applied": applied,
        })
        .to_string();
        let _ = s.events.send(sse_json);
    }

    Ok(Json(ApplyResponse { applied, skipped }))
}

async fn post_comment(
    State(s): State<Arc<AppState>>,
    Path(id_hex): Path<String>,
    Json(comment): Json<Comment>,
) -> Result<Json<serde_json::Value>, AppError> {
    let h = mosaic_core::Hash::from_hex(&id_hex)?;
    if comment.change != ChangeId(h) {
        return Err(AppError::Forbidden(
            "comment.change does not match URL".into(),
        ));
    }
    let _guard = s.lock.lock().await;
    let repo = s.open()?;
    let cid = repo.add_comment(&comment)?;
    Ok(Json(serde_json::json!({ "id": cid.to_hex() })))
}

async fn list_comments(
    State(s): State<Arc<AppState>>,
    Path(id_hex): Path<String>,
) -> Result<Json<Vec<Comment>>, AppError> {
    let repo = s.open()?;
    let h = mosaic_core::Hash::from_hex(&id_hex)?;
    Ok(Json(repo.comments_for(&ChangeId(h))?))
}

async fn post_approval(
    State(s): State<Arc<AppState>>,
    Path(id_hex): Path<String>,
    Json(approval): Json<Approval>,
) -> Result<Json<serde_json::Value>, AppError> {
    let h = mosaic_core::Hash::from_hex(&id_hex)?;
    if approval.change != ChangeId(h) {
        return Err(AppError::Forbidden(
            "approval.change does not match URL".into(),
        ));
    }
    let _guard = s.lock.lock().await;
    let repo = s.open()?;
    let aid = repo.add_approval(&approval)?;
    Ok(Json(serde_json::json!({ "id": aid.to_hex() })))
}

async fn list_approvals(
    State(s): State<Arc<AppState>>,
    Path(id_hex): Path<String>,
) -> Result<Json<Vec<Approval>>, AppError> {
    let repo = s.open()?;
    let h = mosaic_core::Hash::from_hex(&id_hex)?;
    Ok(Json(repo.approvals_for(&ChangeId(h))?))
}

#[derive(Serialize, Deserialize)]
pub struct IssueSummary {
    pub number: u64,
    pub title: String,
    pub status: IssueStatus,
    pub author: String,
}

async fn list_issues(
    State(s): State<Arc<AppState>>,
) -> Result<Json<Vec<IssueSummary>>, AppError> {
    let repo = s.open()?;
    let mut out = Vec::new();
    for issue in repo.list_issues()? {
        let status = repo.issue_status(issue.number)?;
        out.push(IssueSummary {
            number: issue.number,
            title: issue.title,
            status,
            author: issue.author.display(),
        });
    }
    Ok(Json(out))
}

async fn post_issue(
    State(s): State<Arc<AppState>>,
    Json(issue): Json<Issue>,
) -> Result<Json<serde_json::Value>, AppError> {
    // Verify the on-the-wire payload before persisting; reject tampering.
    issue.verify()?;
    let _guard = s.lock.lock().await;
    let repo = s.open()?;
    repo.put_issue(&issue)?;
    Ok(Json(serde_json::json!({
        "number": issue.number,
        "id": issue.id().to_hex(),
    })))
}

#[derive(Serialize)]
struct IssueDetail {
    issue: Issue,
    status: IssueStatus,
    events: Vec<IssueEvent>,
}

async fn get_issue(
    State(s): State<Arc<AppState>>,
    Path(number): Path<u64>,
) -> Result<Json<serde_json::Value>, AppError> {
    let repo = s.open()?;
    let issue = repo.get_issue(number)?;
    let events = repo.issue_events(number)?;
    let status = repo.issue_status(number)?;
    let detail = IssueDetail {
        issue,
        status,
        events,
    };
    Ok(Json(serde_json::to_value(detail).map_err(|e| {
        AppError::Core(Error::IssueStore(e.to_string()))
    })?))
}

async fn post_issue_event(
    State(s): State<Arc<AppState>>,
    Path(number): Path<u64>,
    Json(event): Json<IssueEvent>,
) -> Result<Json<serde_json::Value>, AppError> {
    if event.number != number {
        return Err(AppError::Forbidden(
            "event.number does not match URL".into(),
        ));
    }
    let _guard = s.lock.lock().await;
    let repo = s.open()?;
    repo.add_issue_event(&event)?;
    Ok(Json(serde_json::json!({ "id": event.id().to_hex() })))
}

// ---------------------------------------------------------------------------
// Local authoring mode
//
// The endpoints above accept a *pre-signed* artifact: the client holds the
// private key and signs in its own process. That's correct for distributed
// peers, but a browser has no key — so the web UI can't author reviews or
// issues through them.
//
// The `/local` variants below close that gap: the server signs the artifact
// with the repository's own stored identity (`repo.load_identity()`), so a
// human reviewing in the dashboard can comment, approve, and file issues
// without a local key. This is "local authoring mode" and is only meaningful
// on a server you control — every artifact it produces is attributed to the
// repo's identity. If the repo has no saved identity the call fails cleanly.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct NewCommentReq {
    body: String,
    #[serde(default)]
    anchor: Option<CommentAnchor>,
    /// Hex change-comment id this comment replies to, if any.
    #[serde(default)]
    reply_to: Option<String>,
}

async fn post_comment_local(
    State(s): State<Arc<AppState>>,
    Path(id_hex): Path<String>,
    Json(req): Json<NewCommentReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let h = mosaic_core::Hash::from_hex(&id_hex)?;
    let _guard = s.lock.lock().await;
    let repo = s.open()?;
    let (idn, key) = repo.load_identity()?;
    let mut b = CommentBuilder::new(ChangeId(h), idn, key).body(req.body);
    if let Some(anchor) = req.anchor {
        b = b.anchor(anchor);
    }
    if let Some(rt) = req.reply_to {
        b = b.reply_to(mosaic_core::Hash::from_hex(&rt)?);
    }
    let comment = b.build()?;
    let cid = repo.add_comment(&comment)?;
    Ok(Json(serde_json::json!({
        "id": cid.to_hex(),
        "author": comment.author.display(),
    })))
}

#[derive(Deserialize)]
struct NewApprovalReq {
    verdict: Verdict,
    #[serde(default)]
    body: Option<String>,
}

async fn post_approval_local(
    State(s): State<Arc<AppState>>,
    Path(id_hex): Path<String>,
    Json(req): Json<NewApprovalReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let h = mosaic_core::Hash::from_hex(&id_hex)?;
    let _guard = s.lock.lock().await;
    let repo = s.open()?;
    let (idn, key) = repo.load_identity()?;
    let mut b = ApprovalBuilder::new(ChangeId(h), idn, key, req.verdict);
    if let Some(body) = req.body {
        b = b.body(body);
    }
    let approval = b.build()?;
    let aid = repo.add_approval(&approval)?;
    Ok(Json(serde_json::json!({
        "id": aid.to_hex(),
        "reviewer": approval.reviewer.display(),
    })))
}

#[derive(Deserialize)]
struct NewIssueReq {
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    labels: Vec<String>,
}

async fn post_issue_local(
    State(s): State<Arc<AppState>>,
    Json(req): Json<NewIssueReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let _guard = s.lock.lock().await;
    let repo = s.open()?;
    let (idn, key) = repo.load_identity()?;
    let issue = repo.create_issue(req.title, req.body, idn, key, req.labels)?;
    Ok(Json(serde_json::json!({
        "number": issue.number,
        "id": issue.id().to_hex(),
        "author": issue.author.display(),
    })))
}

#[derive(Deserialize)]
struct NewIssueEventReq {
    kind: IssueEventKind,
}

async fn post_issue_event_local(
    State(s): State<Arc<AppState>>,
    Path(number): Path<u64>,
    Json(req): Json<NewIssueEventReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let _guard = s.lock.lock().await;
    let repo = s.open()?;
    let (idn, key) = repo.load_identity()?;
    let event = IssueEventBuilder::new(number, idn, key, req.kind).build()?;
    repo.add_issue_event(&event)?;
    Ok(Json(serde_json::json!({ "id": event.id().to_hex() })))
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
            AppError::Core(Error::InvalidComment(m)) => {
                (StatusCode::BAD_REQUEST, format!("invalid comment: {m}"))
            }
            AppError::Core(Error::InvalidApproval(m)) => {
                (StatusCode::BAD_REQUEST, format!("invalid approval: {m}"))
            }
            AppError::Core(Error::InvalidIssue(m)) => {
                (StatusCode::BAD_REQUEST, format!("invalid issue: {m}"))
            }
            AppError::Core(Error::IssueNotFound(n)) => {
                (StatusCode::NOT_FOUND, format!("issue not found: {n}"))
            }
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

/// Start the server on `addr` with TLS, terminating HTTPS in-process using a
/// PEM certificate chain and private key. Returns the bound address (so tests
/// can bind port 0 and discover the port) and a handle to the serving task.
///
/// This is native TLS — no reverse proxy required. The certificate and key are
/// standard PEM files (`--tls-cert` / `--tls-key` on `mosaic-serve`).
pub async fn serve_tls(
    repo_root: impl Into<PathBuf>,
    addr: std::net::SocketAddr,
    cert_pem: impl Into<PathBuf>,
    key_pem: impl Into<PathBuf>,
) -> Result<(std::net::SocketAddr, tokio::task::JoinHandle<()>), Box<dyn std::error::Error + Send + Sync>>
{
    // rustls 0.23 requires a process-wide crypto provider. Install ring's
    // (idempotent — ignore the error if another component already did it).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let config =
        axum_server::tls_rustls::RustlsConfig::from_pem_file(cert_pem.into(), key_pem.into())
            .await?;

    let state = Arc::new(AppState::new(repo_root));
    let app = router(state);
    let handle = axum_server::Handle::new();
    let serve_handle = handle.clone();
    let task = tokio::spawn(async move {
        let _ = axum_server::bind_rustls(addr, config)
            .handle(serve_handle)
            .serve(app.into_make_service())
            .await;
    });
    let bound = handle
        .listening()
        .await
        .ok_or("TLS server failed to bind")?;
    Ok((bound, task))
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
    async fn diff_endpoint_marks_inserts_deletes_and_equals() {
        let dir = TempDir::new().unwrap();
        let _ = Repository::init(dir.path()).unwrap();
        let (idn, key) = human();
        let (first_id, second_id) = {
            let mut repo = Repository::open(dir.path()).unwrap();
            let first = ChangeBuilder::new(idn.clone(), key.clone())
                .intent("initial")
                .file(FileChange {
                    path: "file.txt".into(),
                    kind: FileKind::Text,
                    patch: b"alpha\nbeta\ngamma\n".to_vec(),
                    conflicts: Vec::new(),
                })
                .build()
                .unwrap();
            let first_id = repo.commit(first).unwrap();
            repo.advance_branch("main", first_id).unwrap();

            let second = ChangeBuilder::new(idn.clone(), key.clone())
                .intent("edit")
                .dep(first_id)
                .file(FileChange {
                    path: "file.txt".into(),
                    kind: FileKind::Text,
                    patch: b"alpha\nBETA\ngamma\ndelta\n".to_vec(),
                    conflicts: Vec::new(),
                })
                .build()
                .unwrap();
            let second_id = repo.commit(second).unwrap();
            repo.advance_branch("main", second_id).unwrap();
            (first_id, second_id)
        };

        let (addr, handle) =
            serve(dir.path(), "127.0.0.1:0".parse().unwrap()).await.unwrap();
        let url = format!("http://{addr}/api/v1/changes/{}/diff", second_id.to_hex());
        let resp: Vec<FileDiff> = reqwest::get(&url).await.unwrap().json().await.unwrap();
        assert_eq!(resp.len(), 1);
        let file = &resp[0];
        assert_eq!(file.path, "file.txt");
        assert_eq!(file.status, "modified");

        let tags: Vec<&str> = file.hunks.iter().map(|h| h.tag.as_str()).collect();
        assert!(tags.contains(&"delete"), "expected a delete hunk for beta");
        assert!(tags.contains(&"insert"), "expected an insert hunk");

        let insert_lines: Vec<&str> = file
            .hunks
            .iter()
            .filter(|h| h.tag == "insert")
            .map(|h| h.line.as_str())
            .collect();
        assert!(insert_lines.iter().any(|l| *l == "BETA"));
        assert!(insert_lines.iter().any(|l| *l == "delta"));

        // First change has no parents -> all lines marked "insert", status "added".
        let url = format!("http://{addr}/api/v1/changes/{}/diff", first_id.to_hex());
        let resp: Vec<FileDiff> = reqwest::get(&url).await.unwrap().json().await.unwrap();
        assert_eq!(resp[0].status, "added");
        assert!(resp[0].hunks.iter().all(|h| h.tag == "insert"));

        handle.abort();
    }

    #[tokio::test]
    async fn graph_endpoint_returns_topologically_ordered_nodes() {
        let dir = TempDir::new().unwrap();
        let _ = Repository::init(dir.path()).unwrap();
        let (idn, key) = human();
        let (a, b, c) = {
            let mut repo = Repository::open(dir.path()).unwrap();
            let a = one_commit(&mut repo, &idn, &key, "a");
            let b_change = ChangeBuilder::new(idn.clone(), key.clone())
                .intent("b")
                .dep(a)
                .file(FileChange {
                    path: "b.txt".into(),
                    kind: FileKind::Text,
                    patch: b"b".to_vec(),
                    conflicts: Vec::new(),
                })
                .build()
                .unwrap();
            let b = repo.commit(b_change).unwrap();
            repo.advance_branch("main", b).unwrap();
            let c_change = ChangeBuilder::new(idn.clone(), key.clone())
                .intent("c")
                .dep(b)
                .file(FileChange {
                    path: "c.txt".into(),
                    kind: FileKind::Text,
                    patch: b"c".to_vec(),
                    conflicts: Vec::new(),
                })
                .build()
                .unwrap();
            let c = repo.commit(c_change).unwrap();
            repo.advance_branch("main", c).unwrap();
            (a, b, c)
        };

        let (addr, handle) =
            serve(dir.path(), "127.0.0.1:0".parse().unwrap()).await.unwrap();
        let url = format!("http://{addr}/api/v1/graph");
        let body: BranchGraph = reqwest::get(&url).await.unwrap().json().await.unwrap();

        assert_eq!(body.nodes.len(), 3);
        assert_eq!(body.nodes[0].id, a.to_hex());
        assert_eq!(body.nodes[1].id, b.to_hex());
        assert_eq!(body.nodes[2].id, c.to_hex());
        assert!(body.lane_count >= 1);

        // Tip should carry the "main" branch label.
        assert!(body.nodes[2].branches.iter().any(|n| n == "main"));
        // Root has no deps.
        assert!(body.nodes[0].deps.is_empty());
        // Middle node has a's hash as parent.
        assert_eq!(body.nodes[1].deps, vec![a.to_hex()]);
        // Depth grows along the chain.
        assert!(body.nodes[2].depth >= body.nodes[1].depth);
        assert!(body.nodes[1].depth >= body.nodes[0].depth);

        handle.abort();
    }

    #[tokio::test]
    async fn signaling_routes_directed_messages_and_broadcasts_otherwise() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message as WsMsg;

        let dir = TempDir::new().unwrap();
        let _ = Repository::init(dir.path()).unwrap();
        let (addr, handle) =
            serve(dir.path(), "127.0.0.1:0".parse().unwrap()).await.unwrap();
        let url = format!("ws://{addr}/ws/signal/call-1");

        let (mut alice, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let alice_hello: serde_json::Value = serde_json::from_str(
            match alice.next().await.unwrap().unwrap() {
                WsMsg::Text(t) => t,
                other => panic!("expected text hello, got {other:?}"),
            }
            .as_str(),
        )
        .unwrap();
        let alice_id = alice_hello["peer"].as_u64().unwrap();

        let (mut bob, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let bob_hello: serde_json::Value = serde_json::from_str(
            match bob.next().await.unwrap().unwrap() {
                WsMsg::Text(t) => t,
                other => panic!("expected text hello, got {other:?}"),
            }
            .as_str(),
        )
        .unwrap();
        let bob_id = bob_hello["peer"].as_u64().unwrap();

        // Alice received a join notification for Bob.
        let join_for_alice: serde_json::Value = serde_json::from_str(
            match alice.next().await.unwrap().unwrap() {
                WsMsg::Text(t) => t,
                other => panic!("expected text, got {other:?}"),
            }
            .as_str(),
        )
        .unwrap();
        assert_eq!(join_for_alice["type"], "join");
        assert_eq!(join_for_alice["peer"], bob_id);

        let (mut carol, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let carol_hello: serde_json::Value = serde_json::from_str(
            match carol.next().await.unwrap().unwrap() {
                WsMsg::Text(t) => t,
                other => panic!("expected text hello, got {other:?}"),
            }
            .as_str(),
        )
        .unwrap();
        let carol_id = carol_hello["peer"].as_u64().unwrap();
        // Drain alice + bob's "carol joined" messages.
        let _ = alice.next().await;
        let _ = bob.next().await;

        // Alice sends an offer DIRECTED at Bob; Carol must NOT receive it.
        let offer = serde_json::json!({
            "type": "offer",
            "to": bob_id,
            "sdp": "v=0...",
        })
        .to_string();
        alice.send(WsMsg::Text(offer)).await.unwrap();

        let bob_msg: serde_json::Value = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            async {
                loop {
                    match bob.next().await.unwrap().unwrap() {
                        WsMsg::Text(t) => {
                            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                            if v["type"] == "offer" {
                                return v;
                            }
                        }
                        _ => continue,
                    }
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(bob_msg["from"].as_u64().unwrap(), alice_id);
        assert_eq!(bob_msg["sdp"], "v=0...");

        // Carol should NOT see the directed message in 200ms.
        let carol_got = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            carol.next(),
        )
        .await;
        assert!(
            carol_got.is_err(),
            "carol received a message that was directed only at bob"
        );

        // Now alice sends a BROADCAST (no "to" field). Both bob and carol
        // see it; alice does NOT see her own message back.
        let bcast = serde_json::json!({
            "type": "ice-candidate",
            "candidate": "anyone listening?",
        })
        .to_string();
        alice.send(WsMsg::Text(bcast)).await.unwrap();

        let bob_bcast = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            async {
                loop {
                    match bob.next().await.unwrap().unwrap() {
                        WsMsg::Text(t) => {
                            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                            if v["type"] == "ice-candidate" {
                                return v;
                            }
                        }
                        _ => continue,
                    }
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(bob_bcast["from"].as_u64().unwrap(), alice_id);

        let carol_bcast = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            async {
                loop {
                    match carol.next().await.unwrap().unwrap() {
                        WsMsg::Text(t) => {
                            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                            if v["type"] == "ice-candidate" {
                                return v;
                            }
                        }
                        _ => continue,
                    }
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(carol_bcast["from"].as_u64().unwrap(), alice_id);

        // Alice should not loop her own broadcast back.
        let alice_loop = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            alice.next(),
        )
        .await;
        assert!(
            alice_loop.is_err(),
            "alice's broadcast looped back to herself"
        );

        let _ = alice.close(None).await;
        let _ = bob.close(None).await;
        let _ = carol.close(None).await;
        handle.abort();

        // Reference unused locals to silence warnings.
        let _ = carol_id;
    }

    #[tokio::test]
    async fn awareness_tags_messages_with_server_assigned_peer_id() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message as WsMsg;

        let dir = TempDir::new().unwrap();
        let _ = Repository::init(dir.path()).unwrap();
        let (addr, handle) =
            serve(dir.path(), "127.0.0.1:0".parse().unwrap()).await.unwrap();

        let url = format!("ws://{addr}/ws/awareness/payments.rs");
        let (mut alice, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (mut bob, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Each peer should first receive its own "hello" with a server-assigned id.
        let alice_hello: serde_json::Value = serde_json::from_str(
            match alice.next().await.unwrap().unwrap() {
                WsMsg::Text(t) => t,
                other => panic!("expected text hello, got {other:?}"),
            }
            .as_str(),
        )
        .unwrap();
        assert_eq!(alice_hello["type"], "hello");
        let alice_peer_id = alice_hello["peer"].as_u64().unwrap();

        let bob_hello: serde_json::Value = serde_json::from_str(
            match bob.next().await.unwrap().unwrap() {
                WsMsg::Text(t) => t,
                other => panic!("expected text hello, got {other:?}"),
            }
            .as_str(),
        )
        .unwrap();
        assert_eq!(bob_hello["type"], "hello");
        let bob_peer_id = bob_hello["peer"].as_u64().unwrap();
        assert_ne!(alice_peer_id, bob_peer_id);

        // Alice sends a presence update; even if she lies about peer_id the
        // server overwrites it with her authoritative id.
        alice
            .send(WsMsg::Text(
                r#"{"type":"cursor","peer":999,"line":42,"col":7}"#.into(),
            ))
            .await
            .unwrap();

        let received = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            async {
                loop {
                    match bob.next().await.unwrap().unwrap() {
                        WsMsg::Text(t) => {
                            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                            if v["type"] == "cursor" {
                                return v;
                            }
                        }
                        _ => continue,
                    }
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(received["peer"].as_u64().unwrap(), alice_peer_id);
        assert_eq!(received["line"], 42);
        assert_eq!(received["col"], 7);

        // Closing Alice's connection should emit a leave message.
        let _ = alice.close(None).await;
        let leave = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            async {
                loop {
                    match bob.next().await.unwrap().unwrap() {
                        WsMsg::Text(t) => {
                            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                            if v["type"] == "leave" {
                                return v;
                            }
                        }
                        _ => continue,
                    }
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(leave["peer"].as_u64().unwrap(), alice_peer_id);

        let _ = bob.close(None).await;
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
    async fn http_post_then_get_comment_roundtrip() {
        use mosaic_core::hash::Hash;
        use mosaic_core::m1::change::Tai64N;
        use mosaic_core::review::{CommentAnchor, CommentBuilder};

        let dir = TempDir::new().unwrap();
        let _ = Repository::init(dir.path()).unwrap();
        let (idn, key) = human();

        let cid = {
            let mut repo = Repository::open(dir.path()).unwrap();
            one_commit(&mut repo, &idn, &key, "first")
        };

        let (addr, handle) =
            serve(dir.path(), "127.0.0.1:0".parse().unwrap()).await.unwrap();

        let author_key = SigningKey::generate();
        let author = Identity::human("reviewer@example.com", Some("Rev".into())).unwrap();
        let comment = CommentBuilder::new(cid, author, author_key)
            .ts(Tai64N(1234, 0))
            .anchor(CommentAnchor::Line {
                path: "first.txt".into(),
                line: 1,
            })
            .body("nit: rename this")
            .build()
            .unwrap();

        let url = format!("http://{addr}/api/v1/changes/{}/comments", cid.to_hex());
        let resp = reqwest::Client::new()
            .post(&url)
            .json(&comment)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        let returned_hex = body["id"].as_str().unwrap();
        let returned = Hash::from_hex(returned_hex).unwrap();
        assert_eq!(returned, comment.id());

        let resp: Vec<Comment> = reqwest::get(&url).await.unwrap().json().await.unwrap();
        assert_eq!(resp.len(), 1);
        assert_eq!(resp[0].body, "nit: rename this");
        assert_eq!(resp[0].change, cid);

        handle.abort();
    }

    #[tokio::test]
    async fn http_rejects_tampered_comment_with_403() {
        use mosaic_core::m1::change::Tai64N;
        use mosaic_core::review::{CommentAnchor, CommentBuilder};

        let dir = TempDir::new().unwrap();
        let _ = Repository::init(dir.path()).unwrap();
        let (idn, key) = human();

        let cid = {
            let mut repo = Repository::open(dir.path()).unwrap();
            one_commit(&mut repo, &idn, &key, "first")
        };

        let (addr, handle) =
            serve(dir.path(), "127.0.0.1:0".parse().unwrap()).await.unwrap();

        let author_key = SigningKey::generate();
        let author = Identity::human("reviewer@example.com", Some("Rev".into())).unwrap();
        let mut comment = CommentBuilder::new(cid, author, author_key)
            .ts(Tai64N(1234, 0))
            .anchor(CommentAnchor::Change)
            .body("original")
            .build()
            .unwrap();
        // Tamper after signing — the on-the-wire payload no longer matches the sig.
        comment.body = "TAMPERED".into();

        let url = format!("http://{addr}/api/v1/changes/{}/comments", cid.to_hex());
        let resp = reqwest::Client::new()
            .post(&url)
            .json(&comment)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // GET still returns empty list — nothing was persisted.
        let listed: Vec<Comment> = reqwest::get(&url).await.unwrap().json().await.unwrap();
        assert!(listed.is_empty());

        handle.abort();
    }

    #[tokio::test]
    async fn http_post_then_get_issue_roundtrip() {
        use mosaic_core::issues::IssueBuilder;
        use mosaic_core::m1::change::Tai64N;

        let dir = TempDir::new().unwrap();
        let _ = Repository::init(dir.path()).unwrap();

        let (addr, handle) =
            serve(dir.path(), "127.0.0.1:0".parse().unwrap()).await.unwrap();

        let author_key = SigningKey::generate();
        let author = Identity::human("filer@example.com", Some("Filer".into())).unwrap();
        let issue = IssueBuilder::new(1, author, author_key)
            .ts(Tai64N(1234, 0))
            .title("login crashes")
            .body("repro steps")
            .label("bug")
            .build()
            .unwrap();

        let url = format!("http://{addr}/api/v1/issues");
        let resp = reqwest::Client::new()
            .post(&url)
            .json(&issue)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["number"].as_u64().unwrap(), 1);

        // List shows the issue with Open status.
        let listed: Vec<IssueSummary> = reqwest::get(&url).await.unwrap().json().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].number, 1);
        assert_eq!(listed[0].title, "login crashes");
        assert_eq!(listed[0].status, IssueStatus::Open);

        // GET it back with its (empty) event stream.
        let detail_url = format!("http://{addr}/api/v1/issues/1");
        let detail: serde_json::Value =
            reqwest::get(&detail_url).await.unwrap().json().await.unwrap();
        assert_eq!(detail["issue"]["title"], "login crashes");
        assert_eq!(detail["status"], "Open");
        assert!(detail["events"].as_array().unwrap().is_empty());

        handle.abort();
    }

    #[tokio::test]
    async fn http_rejects_tampered_issue_with_403() {
        use mosaic_core::issues::IssueBuilder;
        use mosaic_core::m1::change::Tai64N;

        let dir = TempDir::new().unwrap();
        let _ = Repository::init(dir.path()).unwrap();

        let (addr, handle) =
            serve(dir.path(), "127.0.0.1:0".parse().unwrap()).await.unwrap();

        let author_key = SigningKey::generate();
        let author = Identity::human("filer@example.com", Some("Filer".into())).unwrap();
        let mut issue = IssueBuilder::new(1, author, author_key)
            .ts(Tai64N(1234, 0))
            .title("original")
            .body("body")
            .build()
            .unwrap();
        // Tamper after signing — payload no longer matches the signature.
        issue.title = "TAMPERED".into();

        let url = format!("http://{addr}/api/v1/issues");
        let resp = reqwest::Client::new()
            .post(&url)
            .json(&issue)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Nothing was persisted.
        let listed: Vec<IssueSummary> = reqwest::get(&url).await.unwrap().json().await.unwrap();
        assert!(listed.is_empty());

        handle.abort();
    }

    #[tokio::test]
    async fn sse_emits_push_event() {
        use futures_util::StreamExt;

        let server_dir = TempDir::new().unwrap();
        let _ = Repository::init(server_dir.path()).unwrap();
        let (addr, handle) =
            serve(server_dir.path(), "127.0.0.1:0".parse().unwrap()).await.unwrap();
        let base = format!("http://{addr}");

        // Open the SSE stream first.
        let resp = reqwest::Client::new()
            .get(format!("{base}/api/v1/events"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert!(resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("text/event-stream"));
        let mut stream = resp.bytes_stream();

        // Give the subscription a moment to register, then push.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (idn, key) = human();
        let bundle = {
            let bundle_dir = TempDir::new().unwrap();
            let mut repo = Repository::init(bundle_dir.path()).unwrap();
            let cid = one_commit(&mut repo, &idn, &key, "via-sse");
            mosaic_core::sync::build_bundle_for_branch(&repo, "main", &[cid]).unwrap()
        };
        reqwest::Client::new()
            .post(format!("{base}/api/v1/bundle"))
            .body(bundle.encode().unwrap())
            .send()
            .await
            .unwrap();

        // Read until we see a push event.
        let got = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let mut buf = String::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.unwrap();
                buf.push_str(&String::from_utf8_lossy(&chunk));
                if buf.contains("\"type\":\"push\"") {
                    return buf.clone();
                }
            }
            buf
        })
        .await
        .unwrap();
        assert!(got.contains("\"type\":\"push\""), "no push event seen: {got}");
        assert!(got.contains("\"branch\":\"main\""));

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

    // Self-signed cert/key (CN=localhost, SAN IP:127.0.0.1), generated for
    // tests only — never used outside this suite.
    const TEST_CERT_PEM: &str = "-----BEGIN CERTIFICATE-----\n\
MIIDJTCCAg2gAwIBAgIUOejZ9dZ95R0u5iIy0YPORfYYgzkwDQYJKoZIhvcNAQEL\n\
BQAwFDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MDUyMjExMzg1MFoXDTM2MDUx\n\
OTExMzg1MFowFDESMBAGA1UEAwwJbG9jYWxob3N0MIIBIjANBgkqhkiG9w0BAQEF\n\
AAOCAQ8AMIIBCgKCAQEA2PPKASYW2bK6071aIlArEIt7+IGBOaEHeQvvSUY39N9/\n\
IzU1kS26h7PDT/eZmpEi7llVRyJcAWbHXQNeU1fTBRvtbiRx2STKi9mYfDeFfxg/\n\
WMTd35M9dnIZKxhDRLQK7bKyYE567QZQHUTcwErAJ539OJcle/jHngwwzGOrpiTx\n\
rB/M1iwZFGKzGy9BtILwWYwne9R6VdxMsCWjLZvRrp643iM2uPG5Dp5JB1o+HcqC\n\
SdGvTPIxP+WTgb2ZTWpisCWvEkQxj8A3EeQqOBJCoBtyxsJ+I6y+Jd90TrtPu6dI\n\
GuF5iaFHKe1LgcxfaLERgS5jhuwUCZ+RUf/b7HR0KQIDAQABo28wbTAdBgNVHQ4E\n\
FgQUvur02wsIDRlVcSBnk2cmVBlJxcswHwYDVR0jBBgwFoAUvur02wsIDRlVcSBn\n\
k2cmVBlJxcswDwYDVR0TAQH/BAUwAwEB/zAaBgNVHREEEzARhwR/AAABgglsb2Nh\n\
bGhvc3QwDQYJKoZIhvcNAQELBQADggEBAB/TdKDFt4o6N4JLLoGPjat216oI2MuW\n\
TQ7rIZKk66hV3kJN2UTrgrR3KvRtlGvlYKiOv0xIeD6kLOQ0gNcrMBOf4bs1Fpul\n\
/9LLtMb9EH4XXRfF3kmtfUROoqEjMpAHw3P9dZvoABDw5Au7Hf29waFQccc3MJxX\n\
JYW3j/2qwR5rsC/wZjq5LjKWrAGaqyrwuSB+1hSdThP570q2VPxM3A2C5BJAkisK\n\
8LoZQ+DfQYUcZTJ2GUT0ZDSbf32krQ/fdDuOsMlqVOefPl79yD79J5HhRe6/uLzb\n\
KmaZqgij4Mmd6dsscdKwR95aGHFz24vxG46kXnlfWFVrnQyADYHbVSg=\n\
-----END CERTIFICATE-----\n";

    const TEST_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDY88oBJhbZsrrT\n\
vVoiUCsQi3v4gYE5oQd5C+9JRjf0338jNTWRLbqHs8NP95makSLuWVVHIlwBZsdd\n\
A15TV9MFG+1uJHHZJMqL2Zh8N4V/GD9YxN3fkz12chkrGENEtArtsrJgTnrtBlAd\n\
RNzASsAnnf04lyV7+MeeDDDMY6umJPGsH8zWLBkUYrMbL0G0gvBZjCd71HpV3Eyw\n\
JaMtm9GunrjeIza48bkOnkkHWj4dyoJJ0a9M8jE/5ZOBvZlNamKwJa8SRDGPwDcR\n\
5Co4EkKgG3LGwn4jrL4l33ROu0+7p0ga4XmJoUcp7UuBzF9osRGBLmOG7BQJn5FR\n\
/9vsdHQpAgMBAAECggEALTQGd9zO0AcsZCfE2vdnMahOaUXaff5uRytUbSkDSbMz\n\
k0tn4NrtTY8H9+Z4C7uH0q+sVAj1sJkQmvGzupvG7P6XpuZTDlJbHW52FhOfbg7I\n\
TB+gtw+/s6ksU01X3r3AtSwRfH19oVs6YA7UDADHLrn9Y8giWEVKmkSh+kQeJJyV\n\
xRr3L4IcLDcwCQ2VPl/f5zldEdwsftt18TjLHAfbvihs1+1Uev5oxRDEsrF/n7NI\n\
o+BgMcqn7r1kUdCnM+u8vx9ybOyY+5Sf857tcWPdJoe7V/otJOgypR4Pw5h1jg9x\n\
k51OedDyhYF01hLshDonUv9mE2wFELaKBPcGIxSe8QKBgQDrtL02hC0110f+waIA\n\
Has9dAlU1YJZU4vnDoSd4i7qfVzrqNLRSaWhMXhUN4uEHMDCeq6YIojTH/PUWygR\n\
LuUniwA2uG9AFzRw3OK0U8NWdLEtA7wIbpLZnfagYGDdQirmf11aw0X1zfbIJAu9\n\
6MdaRBfl1QJFWX3tYMq51ikkDQKBgQDrobG8DPE5/zroifcs2uUBJcq7ysMFX9QV\n\
raa2uezN4fhF5kBvLXBH3itXYOk/pxtnOSLm1Lf2l64MLmI4CXfGww4BpvWfS9va\n\
v/wY9BaOiT/nsPYg1cDUqheRcHob9n7hrAj2/WkqgWcMm7fYikSTWkiCLdV5yN33\n\
V2G7Z2a9jQKBgQCne74XRsR5RYe61gwu2OYcvJ8E0NHWdy8p9370UQvVQ08LhOKI\n\
JDS03VoLPYy9S1EM3++/2oouur2fX0aRLylVd8enGlayy8pPiCTuzbY3cKOUwNqT\n\
gz6Fs2DThKhPj/y73DSRkb/ccYWxoStWvlkpIsl4XmtGq9h3HBfxBOQm4QKBgC9a\n\
4LBtXXGNdNZVG+Lc3xc69CKHnmgPGT1+F7ozZX7/AflyS9LMK/uVj9pQtK/BMsWs\n\
+vGvIIWjeCwkikK+zF6axs7YMhbglP/Cg7S0IXBl7vzuWJjCvK1AvdnR5AiIonlS\n\
LL8OsLsFJKOpC+qt5xhCFb5r3bJLByj1W8PhBQnlAoGBAMoCTBaTfllXABjInbqp\n\
oEkoK6dXz5yJ6OHYlZ4nHeLTCZgiBhlfWrlx73Eb8lrlq6EUJ7lu0vusGQHJYfTe\n\
/Jgeen9oqSp9oeZQN9nTp9nGWTWnomihQbYaXQdKJFYkeHGR1pe+9HJ4TZ+oFRdh\n\
gZRFGN8znbXqBffFTIqmD/iJ\n\
-----END PRIVATE KEY-----\n";

    #[tokio::test]
    async fn tls_serves_health_over_https() {
        let dir = TempDir::new().unwrap();
        let _ = Repository::init(dir.path()).unwrap();
        let (idn, key) = human();
        {
            let mut repo = Repository::open(dir.path()).unwrap();
            one_commit(&mut repo, &idn, &key, "first");
        }

        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, TEST_CERT_PEM).unwrap();
        std::fs::write(&key_path, TEST_KEY_PEM).unwrap();

        let (bound, task) = serve_tls(
            dir.path(),
            "127.0.0.1:0".parse().unwrap(),
            &cert_path,
            &key_path,
        )
        .await
        .unwrap();

        // Self-signed cert -> accept invalid certs for the test client only.
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();
        let resp = client
            .get(format!("https://{bound}/api/v1/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: HealthResponse = resp.json().await.unwrap();
        assert!(body.ok);
        assert_eq!(body.changes, 1);

        task.abort();
    }

    #[tokio::test]
    async fn local_authoring_signs_with_repo_identity() {
        let dir = TempDir::new().unwrap();
        let _ = Repository::init(dir.path()).unwrap();
        let (idn, key) = human();

        let cid = {
            let mut repo = Repository::open(dir.path()).unwrap();
            // Local authoring requires a saved repo identity for the server to
            // sign with.
            repo.save_identity(&idn, &key).unwrap();
            one_commit(&mut repo, &idn, &key, "first")
        };

        let (addr, handle) = serve(dir.path(), "127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let base = format!("http://{addr}");
        let http = reqwest::Client::new();

        // Comment authored from the "browser" (no client key).
        let comments_url = format!("{base}/api/v1/changes/{}/comments", cid.to_hex());
        let resp = http
            .post(format!("{comments_url}/local"))
            .json(&serde_json::json!({ "body": "looks good from the web UI" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let fetched: Vec<Comment> = reqwest::get(&comments_url).await.unwrap().json().await.unwrap();
        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].body, "looks good from the web UI");
        // The server signed it with the repo identity — signature verifies.
        fetched[0].verify().unwrap();

        // Approval authored from the browser.
        let approvals_url = format!("{base}/api/v1/changes/{}/approvals", cid.to_hex());
        let resp = http
            .post(format!("{approvals_url}/local"))
            .json(&serde_json::json!({ "verdict": "Approved", "body": "ship it" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let approvals: Vec<Approval> = reqwest::get(&approvals_url).await.unwrap().json().await.unwrap();
        assert_eq!(approvals.len(), 1);
        approvals[0].verify().unwrap();

        // Issue authored from the browser.
        let resp = http
            .post(format!("{base}/api/v1/issues/local"))
            .json(&serde_json::json!({
                "title": "web-filed issue",
                "body": "filed without a local key",
                "labels": ["bug"]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let created: serde_json::Value = resp.json().await.unwrap();
        let number = created["number"].as_u64().unwrap();
        assert_eq!(number, 1);

        // Comment event on that issue, also from the browser.
        let resp = http
            .post(format!("{base}/api/v1/issues/{number}/events/local"))
            .json(&serde_json::json!({ "kind": { "Comment": { "body": "a web reply" } } }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let detail: serde_json::Value = reqwest::get(format!("{base}/api/v1/issues/{number}"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(detail["issue"]["title"], "web-filed issue");
        assert_eq!(detail["events"].as_array().unwrap().len(), 1);

        handle.abort();
    }
}
