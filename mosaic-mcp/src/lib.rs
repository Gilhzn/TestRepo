//! Model Context Protocol (MCP) server for Mosaic.
//!
//! Exposes Mosaic as a set of tools that any MCP-aware AI agent
//! (Claude Code, Claude Desktop, etc.) can discover and call. The server
//! speaks JSON-RPC 2.0 over stdio using LSP-style `Content-Length` framing
//! (the same framing the MCP transport spec uses).
//!
//! Tool catalog:
//!   mosaic_status         — working-tree status against a branch tip
//!   mosaic_log            — recent changes on a branch
//!   mosaic_branches       — list branches
//!   mosaic_change_detail  — load a single Change by id
//!   mosaic_commit         — open a session, stage files, commit
//!   mosaic_merge          — three-way merge a text file
//!   mosaic_explain_merge  — same merge with a human-readable explanation
//!
//! Bring up a server with [`run_stdio`]. Tests drive [`Dispatcher`]
//! in-process — no subprocess spawn required.

use mosaic_core::m1::change::{Change, ChangeId, FileChange, FileKind};
use mosaic_core::repo::Repository;
use mosaic_core::working_copy::{StagedIndex, WorkingCopy};
use mosaic_sdk::{FileMerge, MosaicAgent};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "mosaic-mcp";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// JSON-RPC 2.0 error codes used by the MCP transport.
pub mod error_codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    /// MCP tool execution error.
    pub const TOOL_ERROR: i64 = -32000;
}

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[serde(default)]
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcErrorObj>,
}

#[derive(Debug, Serialize)]
struct JsonRpcErrorObj {
    code: i64,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

fn ok(id: Value, result: Value) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    }
}

fn err(id: Value, code: i64, message: impl Into<String>) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(JsonRpcErrorObj {
            code,
            message: message.into(),
            data: None,
        }),
    }
}

/// In-process dispatcher. Holds no per-connection state — every tool call
/// opens its own short-lived Repository handle so concurrent agents stay
/// safe.
#[derive(Default, Clone)]
pub struct Dispatcher;

impl Dispatcher {
    pub fn new() -> Self {
        Self
    }

    /// Handle a parsed JSON-RPC request. Returns `Some(response)` for a
    /// regular request and `None` for a notification (request without an
    /// id, per JSON-RPC 2.0).
    fn handle(&self, req: JsonRpcRequest) -> Option<JsonRpcResponse> {
        let is_notification = req.id.is_none();
        let id = req.id.clone().unwrap_or(Value::Null);

        if !req.jsonrpc.is_empty() && req.jsonrpc != "2.0" {
            if is_notification {
                return None;
            }
            return Some(err(
                id,
                error_codes::INVALID_REQUEST,
                format!("unsupported jsonrpc version: {}", req.jsonrpc),
            ));
        }

        let resp = match req.method.as_str() {
            "initialize" => Some(self.handle_initialize(id.clone(), &req.params)),
            "initialized" | "notifications/initialized" => None,
            "shutdown" => Some(ok(id.clone(), Value::Null)),
            "exit" => None,
            "tools/list" => Some(ok(id.clone(), tools_catalog_json())),
            "tools/call" => Some(self.handle_tools_call(id.clone(), &req.params)),
            other => Some(err(
                id.clone(),
                error_codes::METHOD_NOT_FOUND,
                format!("unknown method: {other}"),
            )),
        };

        if is_notification {
            None
        } else {
            resp
        }
    }

    fn handle_initialize(&self, id: Value, _params: &Value) -> JsonRpcResponse {
        ok(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "serverInfo": {
                    "name": SERVER_NAME,
                    "version": SERVER_VERSION,
                },
                "capabilities": {
                    "tools": {}
                }
            }),
        )
    }

    fn handle_tools_call(&self, id: Value, params: &Value) -> JsonRpcResponse {
        let name = match params.get("name").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => {
                return err(
                    id,
                    error_codes::INVALID_PARAMS,
                    "tools/call missing 'name'",
                )
            }
        };
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);

        let outcome: Result<Value, ToolError> = match name.as_str() {
            "mosaic_status" => tool_status(&args),
            "mosaic_log" => tool_log(&args),
            "mosaic_branches" => tool_branches(&args),
            "mosaic_change_detail" => tool_change_detail(&args),
            "mosaic_commit" => tool_commit(&args),
            "mosaic_merge" => tool_merge(&args),
            "mosaic_explain_merge" => tool_explain_merge(&args),
            other => {
                return err(
                    id,
                    error_codes::METHOD_NOT_FOUND,
                    format!("unknown tool: {other}"),
                )
            }
        };

        match outcome {
            Ok(value) => ok(id, wrap_tool_result(value)),
            Err(ToolError(msg)) => err(id, error_codes::TOOL_ERROR, msg),
        }
    }
}

struct ToolError(String);

impl<E: std::fmt::Display> From<E> for ToolError {
    fn from(value: E) -> Self {
        ToolError(value.to_string())
    }
}

/// Format a tool's return value as MCP-spec content blocks.
fn wrap_tool_result(value: Value) -> Value {
    let text = match &value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    };
    json!({
        "content": [
            { "type": "text", "text": text }
        ]
    })
}

// ---------- tool argument parsing ----------

fn arg_string(args: &Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ToolError(format!("missing or non-string argument: {key}")))
}

fn arg_string_opt(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

fn arg_u32_opt(args: &Value, key: &str) -> Option<u32> {
    args.get(key).and_then(Value::as_u64).map(|n| n as u32)
}

fn repo_path(args: &Value) -> Result<PathBuf, ToolError> {
    Ok(PathBuf::from(arg_string(args, "repo")?))
}

// ---------- tool implementations ----------

fn tool_status(args: &Value) -> Result<Value, ToolError> {
    let root = repo_path(args)?;
    let branch = arg_string_opt(args, "branch").unwrap_or_else(|| "main".into());
    let repo = Repository::open(&root)?;
    let wc = WorkingCopy::open(&repo, root.clone());
    let index = StagedIndex::load(&root)?;
    let entries = wc.status(&branch, &index)?;
    let json_entries: Vec<Value> = entries
        .into_iter()
        .map(|e| {
            json!({
                "path": e.path,
                "state": format!("{:?}", e.state),
                "staged": e.staged,
            })
        })
        .collect();
    Ok(json!({
        "repo": root.display().to_string(),
        "branch": branch,
        "entries": json_entries,
    }))
}

fn tool_log(args: &Value) -> Result<Value, ToolError> {
    let root = repo_path(args)?;
    let branch = arg_string_opt(args, "branch").unwrap_or_else(|| "main".into());
    let limit = arg_u32_opt(args, "limit");
    let agent = MosaicAgent::attach(&root)?;
    let history = agent.history_on(&branch)?;
    let repo = Repository::open(&root)?;

    let take: Box<dyn Iterator<Item = &ChangeId>> = match limit {
        Some(n) => Box::new(history.iter().rev().take(n as usize)),
        None => Box::new(history.iter().rev()),
    };

    let mut summaries: Vec<Value> = Vec::new();
    for id in take {
        let change = repo.load_change(id)?;
        summaries.push(change_summary_json(id, &change));
    }
    Ok(json!({
        "repo": root.display().to_string(),
        "branch": branch,
        "count": summaries.len(),
        "changes": summaries,
    }))
}

fn change_summary_json(id: &ChangeId, change: &Change) -> Value {
    json!({
        "id": id.to_hex(),
        "intent": change.intent.clone().unwrap_or_default(),
        "author": change.author.display(),
        "ts_seconds": change.ts.0,
        "ts_nanos": change.ts.1,
        "deps": change.deps.iter().map(|d| d.to_hex()).collect::<Vec<_>>(),
        "file_count": change.body.len(),
    })
}

fn tool_branches(args: &Value) -> Result<Value, ToolError> {
    let root = repo_path(args)?;
    let agent = MosaicAgent::attach(&root)?;
    let names = agent.branches()?;
    Ok(json!({
        "repo": root.display().to_string(),
        "branches": names,
    }))
}

fn tool_change_detail(args: &Value) -> Result<Value, ToolError> {
    let root = repo_path(args)?;
    let id_hex = arg_string(args, "change_id")?;
    let hash = mosaic_core::Hash::from_hex(&id_hex)
        .map_err(|e| ToolError(format!("bad change_id hex: {e}")))?;
    let id = ChangeId(hash);
    let repo = Repository::open(&root)?;
    let change = repo.load_change(&id)?;

    let files: Vec<Value> = change
        .body
        .iter()
        .map(|f| file_change_json(f))
        .collect();

    Ok(json!({
        "id": id.to_hex(),
        "intent": change.intent.clone().unwrap_or_default(),
        "author": change.author.display(),
        "ts_seconds": change.ts.0,
        "ts_nanos": change.ts.1,
        "deps": change.deps.iter().map(|d| d.to_hex()).collect::<Vec<_>>(),
        "files": files,
    }))
}

fn file_change_json(fc: &FileChange) -> Value {
    let kind = match fc.kind {
        FileKind::Text => "text",
        FileKind::Binary => "binary",
        FileKind::Tree => "tree",
    };
    let preview = match fc.kind {
        FileKind::Text => Some(String::from_utf8_lossy(&fc.patch).into_owned()),
        _ => None,
    };
    json!({
        "path": fc.path,
        "kind": kind,
        "patch_bytes": fc.patch.len(),
        "preview": preview,
    })
}

#[derive(Debug, Deserialize)]
struct CommitArgs {
    repo: String,
    intent: String,
    files: BTreeMap<String, String>,
    #[serde(default)]
    branch: Option<String>,
}

fn tool_commit(args: &Value) -> Result<Value, ToolError> {
    let parsed: CommitArgs = serde_json::from_value(args.clone())
        .map_err(|e| ToolError(format!("invalid commit arguments: {e}")))?;
    if parsed.files.is_empty() {
        return Err(ToolError("commit requires at least one file".into()));
    }
    let branch = parsed.branch.unwrap_or_else(|| "main".into());
    let agent = MosaicAgent::attach(&parsed.repo)?;
    let mut session = agent.begin_session_on(&branch, &parsed.intent)?;
    let mut paths: Vec<String> = Vec::with_capacity(parsed.files.len());
    for (path, content) in parsed.files {
        paths.push(path.clone());
        session.stage_text(path, content.into_bytes());
    }
    let id = session.commit()?;
    Ok(json!({
        "change": id.to_hex(),
        "branch": branch,
        "files": paths,
    }))
}

#[derive(Debug, Deserialize)]
struct MergeArgs {
    path: String,
    base: String,
    ours: String,
    theirs: String,
}

fn tool_merge(args: &Value) -> Result<Value, ToolError> {
    let parsed: MergeArgs = serde_json::from_value(args.clone())
        .map_err(|e| ToolError(format!("invalid merge arguments: {e}")))?;
    let merge = run_merge(&parsed)?;
    Ok(file_merge_json(&parsed.path, &merge))
}

fn tool_explain_merge(args: &Value) -> Result<Value, ToolError> {
    let parsed: MergeArgs = serde_json::from_value(args.clone())
        .map_err(|e| ToolError(format!("invalid merge arguments: {e}")))?;
    let merge = run_merge(&parsed)?;
    Ok(Value::String(merge.explain()))
}

fn run_merge(parsed: &MergeArgs) -> Result<FileMerge, ToolError> {
    // Use a stand-alone repo-less merge so this tool works without any
    // particular Mosaic repository being open. The Hash::of(path) seed
    // mirrors what MosaicAgent::merge_file does internally.
    let creator = mosaic_core::Hash::of(parsed.path.as_bytes());
    let lang = mosaic_core::ast::Lang::from_path(&parsed.path);
    let merge = mosaic_core::merge_strategies::merge_text_file(
        &creator,
        lang,
        &parsed.base,
        &parsed.ours,
        &parsed.theirs,
    )?;
    Ok(merge)
}

fn file_merge_json(path: &str, m: &FileMerge) -> Value {
    let conflicts: Vec<Value> = m
        .patch_conflicts
        .iter()
        .map(|c| json!({ "summary": format!("{:?}", c) }))
        .collect();
    let hints: Vec<String> = m
        .semantic_hints
        .iter()
        .map(|h| format!("{:?}", h))
        .collect();
    json!({
        "path": path,
        "clean": m.is_clean(),
        "detected_rename": m.detected_a_rename(),
        "merged_text": m.merged_lines.join(""),
        "merged_lines": m.merged_lines.len(),
        "patch_conflicts": conflicts,
        "semantic_hints": hints,
        "renames": m
            .renames
            .iter()
            .map(|(a, b)| json!({ "from": a, "to": b }))
            .collect::<Vec<_>>(),
    })
}

// ---------- catalog ----------

fn tools_catalog_json() -> Value {
    json!({
        "tools": [
            {
                "name": "mosaic_status",
                "description": "Report working-tree status vs a branch tip.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "repo":   { "type": "string", "description": "absolute path to the Mosaic repo" },
                        "branch": { "type": "string", "description": "branch name (default 'main')" }
                    },
                    "required": ["repo"]
                }
            },
            {
                "name": "mosaic_log",
                "description": "List recent Changes on a branch (newest first).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "repo":   { "type": "string" },
                        "branch": { "type": "string" },
                        "limit":  { "type": "integer", "minimum": 1 }
                    },
                    "required": ["repo"]
                }
            },
            {
                "name": "mosaic_branches",
                "description": "List all branches in the repository.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "repo": { "type": "string" }
                    },
                    "required": ["repo"]
                }
            },
            {
                "name": "mosaic_change_detail",
                "description": "Load a single Change by id and list its files.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "repo":      { "type": "string" },
                        "change_id": { "type": "string", "description": "hex-encoded ChangeId" }
                    },
                    "required": ["repo", "change_id"]
                }
            },
            {
                "name": "mosaic_commit",
                "description": "Open a Session, stage a map of {path -> content}, commit one Change.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "repo":   { "type": "string" },
                        "intent": { "type": "string" },
                        "files":  {
                            "type": "object",
                            "additionalProperties": { "type": "string" },
                            "description": "map from in-repo path to UTF-8 content"
                        },
                        "branch": { "type": "string" }
                    },
                    "required": ["repo", "intent", "files"]
                }
            },
            {
                "name": "mosaic_merge",
                "description": "Three-way merge a text file. Returns structured merge result.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path":   { "type": "string" },
                        "base":   { "type": "string" },
                        "ours":   { "type": "string" },
                        "theirs": { "type": "string" }
                    },
                    "required": ["path", "base", "ours", "theirs"]
                }
            },
            {
                "name": "mosaic_explain_merge",
                "description": "Three-way merge a text file, returning a human-readable explanation.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path":   { "type": "string" },
                        "base":   { "type": "string" },
                        "ours":   { "type": "string" },
                        "theirs": { "type": "string" }
                    },
                    "required": ["path", "base", "ours", "theirs"]
                }
            }
        ]
    })
}

// ---------- framing ----------

/// Read one `Content-Length`-framed message from `reader`. Returns `Ok(None)`
/// on clean EOF (no headers).
pub async fn read_message<R: AsyncReadExt + Unpin>(
    reader: &mut BufReader<R>,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut content_length: Option<usize> = None;
    let mut header_line = String::new();
    let mut saw_any_header = false;
    loop {
        header_line.clear();
        let read = reader.read_line(&mut header_line).await?;
        if read == 0 {
            if !saw_any_header {
                return Ok(None);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "EOF inside header block",
            ));
        }
        saw_any_header = true;
        let trimmed = header_line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            let n: usize = rest
                .trim()
                .parse()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            content_length = Some(n);
        }
        // Other headers (e.g. Content-Type) are ignored.
    }
    let n = content_length.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "missing Content-Length header",
        )
    })?;
    let mut buf = vec![0u8; n];
    reader.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

/// Frame a JSON payload with `Content-Length` and write it.
pub async fn write_message<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    payload: &[u8],
) -> std::io::Result<()> {
    let header = format!("Content-Length: {}\r\n\r\n", payload.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Parse one JSON-RPC payload, dispatch it, and return the serialized
/// response (or `None` for notifications). Used by tests and `run_stdio`.
pub fn process_payload(dispatcher: &Dispatcher, payload: &[u8]) -> Option<Vec<u8>> {
    let parsed: Result<JsonRpcRequest, _> = serde_json::from_slice(payload);
    let req = match parsed {
        Ok(r) => r,
        Err(e) => {
            let r = err(
                Value::Null,
                error_codes::PARSE_ERROR,
                format!("parse error: {e}"),
            );
            return Some(serde_json::to_vec(&r).expect("serialize"));
        }
    };
    let resp = dispatcher.handle(req)?;
    Some(serde_json::to_vec(&resp).expect("serialize"))
}

/// Run the MCP server on stdio until EOF.
pub async fn run_stdio() {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let dispatcher = Dispatcher::new();

    loop {
        match read_message(&mut reader).await {
            Ok(Some(bytes)) => {
                if let Some(resp) = process_payload(&dispatcher, &bytes) {
                    if write_message(&mut stdout, &resp).await.is_err() {
                        break;
                    }
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mosaic_core::m1::identity::Identity;
    use mosaic_sdk::SigningKey;
    use tempfile::TempDir;
    use tokio::io::BufReader as TokioBufReader;

    fn dispatch_json(d: &Dispatcher, payload: Value) -> Value {
        let bytes = serde_json::to_vec(&payload).unwrap();
        let out = process_payload(d, &bytes).expect("expected a response");
        serde_json::from_slice(&out).unwrap()
    }

    fn rpc(id: i64, method: &str, params: Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
    }

    fn fresh_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let idn = Identity::human("dev@example.com", Some("Dev".into())).unwrap();
        let _ = MosaicAgent::init(dir.path(), idn, SigningKey::generate()).unwrap();
        dir
    }

    #[test]
    fn initialize_returns_server_info_and_tools_capability() {
        let d = Dispatcher::new();
        let resp = dispatch_json(&d, rpc(1, "initialize", json!({})));
        assert_eq!(resp["id"], json!(1));
        assert_eq!(resp["jsonrpc"], "2.0");
        let result = &resp["result"];
        assert_eq!(result["serverInfo"]["name"], "mosaic-mcp");
        assert_eq!(result["protocolVersion"], PROTOCOL_VERSION);
        assert!(
            result["capabilities"]["tools"].is_object(),
            "tools capability missing: {result}"
        );
    }

    #[test]
    fn tools_list_returns_full_catalog() {
        let d = Dispatcher::new();
        let resp = dispatch_json(&d, rpc(2, "tools/list", json!({})));
        let tools = resp["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 7, "expected 7 tools, got {}", tools.len());
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        for expected in [
            "mosaic_status",
            "mosaic_log",
            "mosaic_branches",
            "mosaic_change_detail",
            "mosaic_commit",
            "mosaic_merge",
            "mosaic_explain_merge",
        ] {
            assert!(names.contains(&expected), "missing tool {expected}: {names:?}");
        }
    }

    #[test]
    fn mosaic_branches_against_fresh_repo() {
        let dir = fresh_repo();
        let d = Dispatcher::new();
        let resp = dispatch_json(
            &d,
            rpc(
                3,
                "tools/call",
                json!({
                    "name": "mosaic_branches",
                    "arguments": { "repo": dir.path().display().to_string() }
                }),
            ),
        );
        assert!(
            resp.get("error").is_none(),
            "expected no error, got {resp}"
        );
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .expect("text payload");
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert!(parsed["branches"].is_array());
    }

    #[test]
    fn mosaic_commit_then_status_round_trip() {
        let dir = fresh_repo();
        let d = Dispatcher::new();

        let commit = dispatch_json(
            &d,
            rpc(
                4,
                "tools/call",
                json!({
                    "name": "mosaic_commit",
                    "arguments": {
                        "repo": dir.path().display().to_string(),
                        "intent": "first via MCP",
                        "files": { "hello.txt": "hello via MCP\n" }
                    }
                }),
            ),
        );
        assert!(
            commit.get("error").is_none(),
            "commit failed: {commit}"
        );
        let commit_text = commit["result"]["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(commit_text).unwrap();
        assert!(parsed["change"].as_str().unwrap().len() > 8);

        // The session.commit does not write to working copy on its own; write
        // it manually so status() sees the file (mirrors how a real agent
        // would also write to disk).
        std::fs::write(dir.path().join("hello.txt"), b"hello via MCP\n").unwrap();

        let log = dispatch_json(
            &d,
            rpc(
                5,
                "tools/call",
                json!({
                    "name": "mosaic_log",
                    "arguments": { "repo": dir.path().display().to_string() }
                }),
            ),
        );
        let log_text = log["result"]["content"][0]["text"].as_str().unwrap();
        let log_parsed: Value = serde_json::from_str(log_text).unwrap();
        assert_eq!(log_parsed["count"], 1);

        let status = dispatch_json(
            &d,
            rpc(
                6,
                "tools/call",
                json!({
                    "name": "mosaic_status",
                    "arguments": { "repo": dir.path().display().to_string() }
                }),
            ),
        );
        assert!(
            status.get("error").is_none(),
            "status returned error: {status}"
        );
        // After committing then writing the same content to disk, hello.txt
        // is unmodified-and-not-staged so it's filtered out. We only assert
        // that the call succeeded and produced a JSON-decodable text block.
        let status_text = status["result"]["content"][0]["text"]
            .as_str()
            .expect("text payload");
        let _: Value = serde_json::from_str(status_text).unwrap();
    }

    #[test]
    fn unknown_tool_returns_method_not_found_error() {
        let d = Dispatcher::new();
        let resp = dispatch_json(
            &d,
            rpc(
                7,
                "tools/call",
                json!({ "name": "nope", "arguments": {} }),
            ),
        );
        let code = resp["error"]["code"].as_i64().unwrap();
        assert_eq!(code, error_codes::METHOD_NOT_FOUND);
    }

    #[test]
    fn unknown_method_returns_method_not_found() {
        let d = Dispatcher::new();
        let resp = dispatch_json(&d, rpc(8, "frobnicate", json!({})));
        assert_eq!(
            resp["error"]["code"].as_i64().unwrap(),
            error_codes::METHOD_NOT_FOUND,
        );
    }

    #[test]
    fn merge_tool_round_trip() {
        let d = Dispatcher::new();
        let resp = dispatch_json(
            &d,
            rpc(
                9,
                "tools/call",
                json!({
                    "name": "mosaic_merge",
                    "arguments": {
                        "path": "x.txt",
                        "base":   "a\nb\nc\n",
                        "ours":   "a\nb\nc\nd\n",
                        "theirs": "a\nb\nc\n",
                    }
                }),
            ),
        );
        assert!(resp.get("error").is_none(), "merge errored: {resp}");
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["clean"], true);
        assert!(parsed["merged_text"].as_str().unwrap().contains("d"));
    }

    #[tokio::test]
    async fn framing_handles_multiple_messages() {
        // Drive three real Content-Length-framed messages through the I/O
        // helpers — initialize + tools/list + an unknown tool.
        let mut buf: Vec<u8> = Vec::new();
        for req in [
            rpc(100, "initialize", json!({})),
            rpc(101, "tools/list", json!({})),
            rpc(
                102,
                "tools/call",
                json!({ "name": "no_such_tool", "arguments": {} }),
            ),
        ] {
            let body = serde_json::to_vec(&req).unwrap();
            let header = format!("Content-Length: {}\r\n\r\n", body.len());
            buf.extend_from_slice(header.as_bytes());
            buf.extend_from_slice(&body);
        }

        let mut reader = TokioBufReader::new(buf.as_slice());
        let d = Dispatcher::new();
        let mut responses = Vec::new();
        while let Some(payload) = read_message(&mut reader).await.unwrap() {
            if let Some(resp) = process_payload(&d, &payload) {
                responses.push(resp);
            }
        }
        assert_eq!(responses.len(), 3, "expected 3 framed responses");

        let r0: Value = serde_json::from_slice(&responses[0]).unwrap();
        assert_eq!(r0["id"], json!(100));
        assert_eq!(r0["result"]["serverInfo"]["name"], "mosaic-mcp");

        let r1: Value = serde_json::from_slice(&responses[1]).unwrap();
        assert_eq!(r1["id"], json!(101));
        assert_eq!(
            r1["result"]["tools"].as_array().unwrap().len(),
            7,
        );

        let r2: Value = serde_json::from_slice(&responses[2]).unwrap();
        assert_eq!(r2["id"], json!(102));
        assert_eq!(
            r2["error"]["code"].as_i64().unwrap(),
            error_codes::METHOD_NOT_FOUND,
        );
    }

    #[tokio::test]
    async fn write_message_emits_lsp_framing() {
        let mut out: Vec<u8> = Vec::new();
        write_message(&mut out, b"{\"ok\":true}").await.unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("Content-Length: 11\r\n\r\n"));
        assert!(s.ends_with("{\"ok\":true}"));
    }

    #[test]
    fn initialized_notification_returns_nothing() {
        let d = Dispatcher::new();
        // No `id` field → notification.
        let payload = json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        });
        let bytes = serde_json::to_vec(&payload).unwrap();
        let out = process_payload(&d, &bytes);
        assert!(out.is_none());
    }

    #[test]
    fn shutdown_returns_null_result() {
        let d = Dispatcher::new();
        let resp = dispatch_json(&d, rpc(11, "shutdown", json!({})));
        assert!(resp.get("result").is_some());
        assert_eq!(resp["result"], Value::Null);
    }
}
