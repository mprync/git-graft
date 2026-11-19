//! MCP stdio adapter for standalone workspaces.
use std::path::PathBuf;

use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
    },
    service::RequestContext,
};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};

use crate::api::{self, AddDep, DropWorkspace, Exec, Gc, Land, Layer, Name, Owned, SeedLayer};
use crate::workspace::{self, Command, Spawn};

const MAX_REQUEST_BYTES: usize = 256 * 1024;

#[derive(Clone)]
struct Server {
    repo: PathBuf,
    allow_exec: bool,
}

#[derive(serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct LockWorkspace {
    #[serde(flatten)]
    workspace: Owned,
    /// Optional explanation for the workspace lock.
    reason: Option<String>,
}

#[derive(serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WorkspacePath {
    #[serde(flatten)]
    workspace: Owned,
    /// Absolute workspace path on the same volume, with a canonical parent.
    path: PathBuf,
}

fn tool<T: JsonSchema>(
    name: &'static str,
    description: &'static str,
    read_only: bool,
    destructive: bool,
) -> Tool {
    let schema = serde_json::to_value(schemars::schema_for!(T)).expect("schema serializes");
    let schema = schema
        .as_object()
        .expect("tool schema is an object")
        .clone();
    Tool::new(name, description, schema).with_annotations(
        ToolAnnotations::new()
            .read_only(read_only)
            .destructive(destructive),
    )
}

fn parse<T: DeserializeOwned>(args: Map<String, Value>) -> Result<T, ErrorData> {
    serde_json::from_value(Value::Object(args))
        .map_err(|e| ErrorData::invalid_params(format!("Invalid tool arguments: {e}"), None))
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        let instructions = if self.allow_exec {
            "Workspace command execution, dependency installation, dependency layers, and lockfile reconciliation are enabled by the host."
        } else {
            "Workspace command execution, dependency installation, dependency layers, and lockfile reconciliation are disabled; the host must start with --allow-exec to enable them."
        };
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(format!(
                "{instructions} Each newline-delimited message may contain up to {MAX_REQUEST_BYTES} bytes. Larger messages are discarded."
            ))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut tools = vec![
            tool::<Empty>(
                "workspace_doctor",
                "Check workspace prerequisites and repository health.",
                true,
                false,
            ),
            tool::<Empty>(
                "workspace_list",
                "List active and recoverable workspaces.",
                true,
                false,
            ),
            tool::<Owned>(
                "workspace_restore",
                "Reopen a retired workspace using its recovery lease.",
                false,
                false,
            ),
            tool::<Owned>(
                "workspace_undo_land",
                "Append a reversal commit if the landed destination is still at the recorded commit.",
                false,
                true,
            ),
            tool::<Spawn>(
                "workspace_spawn",
                "Create a standalone Git repository with a private branch, detached HEAD or unborn branch.",
                false,
                false,
            ),
            tool::<Name>(
                "workspace_status",
                "Inspect a workspace's status.",
                true,
                false,
            ),
            tool::<Owned>(
                "workspace_heartbeat",
                "Refresh a workspace's heartbeat.",
                false,
                false,
            ),
            tool::<Name>(
                "workspace_env",
                "Show the environment for a workspace.",
                true,
                false,
            ),
            tool::<Land>(
                "workspace_land_and_wipe",
                "Land a workspace and archive its repository on success. Lockfile reconciliation requires host --allow-exec.",
                false,
                true,
            ),
            tool::<DropWorkspace>(
                "workspace_discard",
                "Archive a workspace and its files, subject to discard safeguards.",
                false,
                true,
            ),
            tool::<Gc>(
                "workspace_gc",
                "Preview cleanup or archive eligible stale workspaces.",
                false,
                true,
            ),
            tool::<LockWorkspace>(
                "workspace_lock",
                "Protect a workspace against moves, retirement and cleanup.",
                false,
                false,
            ),
            tool::<Owned>(
                "workspace_unlock",
                "Release an explicit workspace lock.",
                false,
                false,
            ),
            tool::<WorkspacePath>(
                "workspace_move",
                "Move a workspace to an available absolute path on the same volume.",
                false,
                false,
            ),
            tool::<WorkspacePath>(
                "workspace_repair",
                "Reconnect a workspace using its absolute path after an external move.",
                false,
                false,
            ),
        ];
        if self.allow_exec {
            tools.push(tool::<Exec>(
                "workspace_exec",
                "Execute a command in a workspace (enabled by host --allow-exec).",
                false,
                true,
            ));
            tools.push(tool::<AddDep>(
                "workspace_add_dependency",
                "Install dependencies in a workspace (enabled by host --allow-exec).",
                false,
                true,
            ));
            tools.push(tool::<Layer>("workspace_layer", "Save, restore or forget an installed dependency layer (enabled by host --allow-exec).", false, true));
            tools.push(tool::<SeedLayer>("workspace_seed_layer", "Seed a relocatable dependency layer from the primary checkout's installed node_modules (enabled by host --allow-exec).", false, false));
        }
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name.as_ref();
        if !self.allow_exec
            && matches!(
                name,
                "workspace_exec"
                    | "workspace_add_dependency"
                    | "workspace_layer"
                    | "workspace_seed_layer"
            )
        {
            let error = anyhow::anyhow!("Tool {name} requires the host to start with --allow-exec");
            return Ok(CallToolResult::structured_error(api::error_json(&error)).into());
        }
        let args = request.arguments.unwrap_or_default();
        let op = match name {
            "workspace_doctor" => {
                parse::<Empty>(args)?;
                Command::Doctor
            }
            "workspace_list" => {
                parse::<Empty>(args)?;
                Command::List
            }
            "workspace_spawn" => {
                Command::Spawn(Spawn::from_mcp(args, !self.allow_exec).map_err(|e| {
                    ErrorData::invalid_params(format!("Invalid tool arguments: {e}"), None)
                })?)
            }
            "workspace_status" => Command::Status(parse(args)?),
            "workspace_heartbeat" => Command::Heartbeat(parse(args)?),
            "workspace_env" => Command::Env(parse(args)?),
            "workspace_exec" => Command::Exec(parse(args)?),
            "workspace_add_dependency" => Command::AddDep(parse(args)?),
            "workspace_layer" => Command::Layer(parse(args)?),
            "workspace_seed_layer" => Command::SeedLayer(parse(args)?),
            "workspace_restore" => Command::Restore(parse(args)?),
            "workspace_undo_land" => Command::UndoLand(parse(args)?),
            "workspace_land_and_wipe" => {
                let land: Land = parse(args)?;
                if land.reconcile && !self.allow_exec {
                    let error = anyhow::anyhow!(
                        "Lockfile reconciliation requires the host to start with --allow-exec"
                    );
                    return Ok(CallToolResult::structured_error(api::error_json(&error)).into());
                }
                Command::Land(land)
            }
            "workspace_discard" => Command::Drop(parse(args)?),
            "workspace_gc" => Command::Gc(parse(args)?),
            "workspace_lock" => {
                let LockWorkspace { workspace, reason } = parse(args)?;
                Command::Lock { workspace, reason }
            }
            "workspace_unlock" => Command::Unlock(parse(args)?),
            "workspace_move" => {
                let WorkspacePath { workspace, path } = parse(args)?;
                Command::Move { workspace, path }
            }
            "workspace_repair" => {
                let WorkspacePath { workspace, path } = parse(args)?;
                Command::Repair { workspace, path }
            }
            _ => {
                return Err(ErrorData::invalid_params(
                    format!("Unknown tool: {name}"),
                    None,
                ));
            }
        };
        let repo = self.repo.clone();
        let result = tokio::task::spawn_blocking(move || workspace::run(&repo, op))
            .await
            .map_err(|e| {
                ErrorData::internal_error(format!("Workspace operation failed to join: {e}"), None)
            })?;
        Ok(match result {
            Ok(value) if value.get("success") == Some(&Value::Bool(false)) => {
                CallToolResult::structured_error(value)
            }
            Ok(value) => CallToolResult::structured(value),
            Err(error) => CallToolResult::structured_error(api::error_json(&error)),
        }
        .into())
    }
}

/// Bound rmcp's stdin framing buffer before it parses or dispatches requests.
fn bounded_input(input: impl AsyncRead + Send + Unpin + 'static) -> impl AsyncRead + Send + Unpin {
    let (mut writer, reader) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let mut input = BufReader::new(input);
        let mut frame = Vec::new();
        let mut oversized = false;
        loop {
            let chunk = input.fill_buf().await?;
            if chunk.is_empty() {
                return Ok::<_, std::io::Error>(());
            }
            let newline = chunk.iter().position(|&byte| byte == b'\n');
            let consumed = newline.map_or(chunk.len(), |index| index + 1);
            oversized |= frame.len() + consumed > MAX_REQUEST_BYTES + 1;
            if !oversized {
                frame.extend_from_slice(&chunk[..consumed]);
            }
            input.consume(consumed);
            if newline.is_some() {
                if !oversized {
                    writer.write_all(&frame).await?;
                }
                frame.clear();
                oversized = false;
            }
        }
    });
    reader
}

/// Serve MCP over stdin/stdout until the client closes the connection.
pub fn serve(repo: PathBuf, allow_exec: bool) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            let (stdin, stdout) = rmcp::transport::stdio();
            Server { repo, allow_exec }
                .serve((bounded_input(stdin), stdout))
                .await?
                .waiting()
                .await?;
            Ok(())
        })
}
