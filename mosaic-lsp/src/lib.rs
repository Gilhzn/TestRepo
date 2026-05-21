//! Language Server Protocol bridge for Mosaic.
//!
//! Surfaces Mosaic state and operations inside any LSP-aware editor
//! (VS Code, Neovim, JetBrains, Helix, Emacs, ...). The server keeps a
//! per-document CRDT shadow of every open file, optionally auto-commits on
//! save, and exposes Mosaic operations as LSP workspace commands so any
//! editor can drive them through its command palette.
//!
//! Workspace commands:
//!   mosaic.commit { uri, intent }    record open file as a Change
//!   mosaic.log                       show recent changes
//!   mosaic.branches                  list branches and their tips
//!   mosaic.status                    report identity + branch state

use mosaic_sdk::{MosaicAgent, Session};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_lsp::jsonrpc::{Error as JsonRpcError, Result as JsonRpcResult};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

pub struct MosaicServer {
    client: Client,
    state: Arc<Mutex<ServerState>>,
}

struct ServerState {
    repo_root: Option<PathBuf>,
    docs: HashMap<Url, String>,
    intents: HashMap<Url, String>,
}

impl ServerState {
    fn new() -> Self {
        Self {
            repo_root: None,
            docs: HashMap::new(),
            intents: HashMap::new(),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct CommitArgs {
    pub uri: Url,
    pub intent: String,
    #[serde(default = "default_branch")]
    pub branch: String,
}

fn default_branch() -> String {
    "main".into()
}

impl MosaicServer {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            state: Arc::new(Mutex::new(ServerState::new())),
        }
    }

    async fn discover_repo(&self, workspace_root: Option<&Path>) -> Option<PathBuf> {
        if let Some(root) = workspace_root {
            let mut cur = Some(root.to_path_buf());
            while let Some(p) = cur {
                if p.join(".mosaic").exists() {
                    return Some(p);
                }
                cur = p.parent().map(Path::to_path_buf);
            }
        }
        None
    }

    fn open_agent(&self, repo_root: &Path) -> Result<MosaicAgent, mosaic_core::Error> {
        MosaicAgent::attach(repo_root)
    }

    async fn handle_commit(&self, args: CommitArgs) -> JsonRpcResult<Value> {
        let state = self.state.lock().await;
        let repo_root = state
            .repo_root
            .clone()
            .ok_or_else(|| internal_error("no Mosaic repo discovered in workspace"))?;
        let snapshot = state
            .docs
            .get(&args.uri)
            .cloned()
            .ok_or_else(|| internal_error("document not open in server"))?;
        let relative = uri_to_relative(&repo_root, &args.uri)
            .ok_or_else(|| internal_error("document is outside the workspace root"))?;
        drop(state);

        let agent = self
            .open_agent(&repo_root)
            .map_err(|e| internal_error(format!("attach: {e}")))?;
        let mut session: Session = agent
            .begin_session_on(&args.branch, &args.intent)
            .map_err(|e| internal_error(format!("session: {e}")))?;
        session.stage_text(relative.clone(), snapshot.into_bytes());
        let id = session
            .commit()
            .map_err(|e| internal_error(format!("commit: {e}")))?;

        let _ = self
            .client
            .show_message(
                MessageType::INFO,
                format!("mosaic: committed {} ({})", &id.to_hex()[..12], relative),
            )
            .await;
        Ok(serde_json::json!({
            "change": id.to_hex(),
            "path": relative,
            "branch": args.branch,
        }))
    }

    async fn handle_log(&self) -> JsonRpcResult<Value> {
        let state = self.state.lock().await;
        let repo_root = state
            .repo_root
            .clone()
            .ok_or_else(|| internal_error("no Mosaic repo"))?;
        drop(state);
        let agent = self
            .open_agent(&repo_root)
            .map_err(|e| internal_error(format!("attach: {e}")))?;
        let history = agent
            .history_on("main")
            .map_err(|e| internal_error(format!("history: {e}")))?;
        Ok(serde_json::json!({
            "branch": "main",
            "changes": history.iter().map(|c| c.to_hex()).collect::<Vec<_>>(),
        }))
    }

    async fn handle_branches(&self) -> JsonRpcResult<Value> {
        let state = self.state.lock().await;
        let repo_root = state
            .repo_root
            .clone()
            .ok_or_else(|| internal_error("no Mosaic repo"))?;
        drop(state);
        let agent = self
            .open_agent(&repo_root)
            .map_err(|e| internal_error(format!("attach: {e}")))?;
        let names = agent
            .branches()
            .map_err(|e| internal_error(format!("branches: {e}")))?;
        Ok(serde_json::json!({ "branches": names }))
    }

    async fn handle_status(&self) -> JsonRpcResult<Value> {
        let state = self.state.lock().await;
        let repo_root = state.repo_root.clone();
        let open_files: Vec<String> = state.docs.keys().map(|u| u.to_string()).collect();
        drop(state);
        let repo_root = repo_root.ok_or_else(|| internal_error("no Mosaic repo"))?;
        let agent = self
            .open_agent(&repo_root)
            .map_err(|e| internal_error(format!("attach: {e}")))?;
        let branches = agent.branches().unwrap_or_default();
        Ok(serde_json::json!({
            "repo": repo_root.display().to_string(),
            "identity": agent.identity().display(),
            "branches": branches,
            "open_files": open_files,
        }))
    }
}

fn uri_to_relative(repo_root: &Path, uri: &Url) -> Option<String> {
    let path = uri.to_file_path().ok()?;
    let rel = path.strip_prefix(repo_root).ok()?;
    Some(rel.to_string_lossy().into_owned())
}

fn internal_error(msg: impl Into<String>) -> JsonRpcError {
    let mut e = JsonRpcError::internal_error();
    e.message = msg.into().into();
    e
}

#[tower_lsp::async_trait]
impl LanguageServer for MosaicServer {
    async fn initialize(&self, params: InitializeParams) -> JsonRpcResult<InitializeResult> {
        let workspace_root: Option<PathBuf> = params
            .workspace_folders
            .as_ref()
            .and_then(|folders| folders.first())
            .and_then(|f| f.uri.to_file_path().ok())
            .or_else(|| {
                #[allow(deprecated)]
                params
                    .root_uri
                    .as_ref()
                    .and_then(|u| u.to_file_path().ok())
            });

        let discovered = self.discover_repo(workspace_root.as_deref()).await;
        {
            let mut state = self.state.lock().await;
            state.repo_root = discovered;
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![
                        "mosaic.commit".into(),
                        "mosaic.log".into(),
                        "mosaic.branches".into(),
                        "mosaic.status".into(),
                    ],
                    work_done_progress_options: Default::default(),
                }),
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo {
                name: "mosaic-lsp".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let state = self.state.lock().await;
        let msg = match &state.repo_root {
            Some(p) => format!("mosaic-lsp ready (repo: {})", p.display()),
            None => "mosaic-lsp ready (no .mosaic/ found in workspace)".into(),
        };
        let _ = self.client.log_message(MessageType::INFO, msg).await;
    }

    async fn shutdown(&self) -> JsonRpcResult<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let mut state = self.state.lock().await;
        state.docs.insert(uri, params.text_document.text);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        if let Some(change) = params.content_changes.into_iter().next() {
            let mut state = self.state.lock().await;
            state.docs.insert(uri, change.text);
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let _ = self
            .client
            .log_message(
                MessageType::INFO,
                format!("saved: {}", params.text_document.uri),
            )
            .await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let mut state = self.state.lock().await;
        state.docs.remove(&params.text_document.uri);
        state.intents.remove(&params.text_document.uri);
    }

    async fn hover(&self, params: HoverParams) -> JsonRpcResult<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let state = self.state.lock().await;
        let repo_root = state.repo_root.clone();
        drop(state);
        let repo_root = match repo_root {
            Some(r) => r,
            None => return Ok(None),
        };
        let agent = match self.open_agent(&repo_root) {
            Ok(a) => a,
            Err(_) => return Ok(None),
        };
        let branches = agent.branches().unwrap_or_default();
        let history = agent.history_on("main").unwrap_or_default();
        let info = format!(
            "**Mosaic**\n\nrepo: `{}`\n\nbranches: {}\n\nchanges on main: {}\n\nfile: `{}`",
            repo_root.display(),
            branches.join(", "),
            history.len(),
            uri,
        );
        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: info,
            }),
            range: None,
        }))
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> JsonRpcResult<Option<Value>> {
        match params.command.as_str() {
            "mosaic.commit" => {
                let args: CommitArgs = params
                    .arguments
                    .into_iter()
                    .next()
                    .and_then(|v| serde_json::from_value(v).ok())
                    .ok_or_else(|| internal_error("missing CommitArgs"))?;
                Ok(Some(self.handle_commit(args).await?))
            }
            "mosaic.log" => Ok(Some(self.handle_log().await?)),
            "mosaic.branches" => Ok(Some(self.handle_branches().await?)),
            "mosaic.status" => Ok(Some(self.handle_status().await?)),
            other => Err(internal_error(format!("unknown command: {other}"))),
        }
    }
}

pub async fn run_stdio() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(MosaicServer::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use mosaic_sdk::SigningKey;
    use mosaic_core::m1::identity::Identity;
    use tempfile::TempDir;

    #[tokio::test]
    async fn handle_status_with_no_repo_errors_cleanly() {
        let (service, _) = LspService::new(MosaicServer::new);
        let inner = service.inner();
        let result = inner.handle_status().await;
        assert!(result.is_err(), "expected error when no repo discovered");
    }

    #[tokio::test]
    async fn handle_commit_round_trip() {
        let dir = TempDir::new().unwrap();
        let idn = Identity::human("dev@example.com", Some("Dev".into())).unwrap();
        let _agent = MosaicAgent::init(dir.path(), idn, SigningKey::generate()).unwrap();

        let (service, _) = LspService::new(MosaicServer::new);
        let inner = service.inner();
        {
            let mut state = inner.state.lock().await;
            state.repo_root = Some(dir.path().to_path_buf());
        }

        let file_path = dir.path().join("hello.txt");
        std::fs::write(&file_path, "hello world\n").unwrap();
        let uri = Url::from_file_path(&file_path).unwrap();

        let mut params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri.clone(),
                language_id: "plaintext".into(),
                version: 1,
                text: "hello world\n".into(),
            },
        };
        inner.did_open(params.clone()).await;
        let _ = &mut params;

        let result = inner
            .handle_commit(CommitArgs {
                uri,
                intent: "first commit via LSP".into(),
                branch: "main".into(),
            })
            .await
            .unwrap();
        assert!(result["change"].as_str().is_some(), "commit returned no change id");
        let history = MosaicAgent::attach(dir.path()).unwrap().history_on("main").unwrap();
        assert_eq!(history.len(), 1);
    }

    #[tokio::test]
    async fn discover_repo_walks_up() {
        let dir = TempDir::new().unwrap();
        let idn = Identity::human("dev@example.com", None).unwrap();
        let _ = MosaicAgent::init(dir.path(), idn, SigningKey::generate()).unwrap();
        let nested = dir.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();

        let (service, _) = LspService::new(MosaicServer::new);
        let inner = service.inner();
        let found = inner.discover_repo(Some(&nested)).await;
        assert_eq!(found.as_deref(), Some(dir.path()));
    }
}
