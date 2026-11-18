//! MCP stdio adapter. All workspace behavior is owned by the shared engine.
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

use crate::api::{
    self, AddDep, DropWorkspace, Exec, Gc, Land, Layer, Name, Operation, Owned, Spawn,
};

#[derive(Clone)]
struct Server {
    repo: PathBuf,
    allow_exec: bool,
}

#[derive(serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Empty {}

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
            .with_instructions(instructions)
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
            tool::<Empty>("workspace_list", "List managed workspaces.", true, false),
            tool::<Spawn>(
                "workspace_spawn",
                "Create a managed workspace.",
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
                "Land a workspace and remove it on success. Lockfile reconciliation requires host --allow-exec.",
                false,
                true,
            ),
            tool::<DropWorkspace>(
                "workspace_discard",
                "Remove a managed workspace, subject to discard safeguards.",
                false,
                true,
            ),
            tool::<Gc>(
                "workspace_gc",
                "Preview or apply cleanup of stale workspaces.",
                false,
                true,
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
                "workspace_exec" | "workspace_add_dependency" | "workspace_layer"
            )
        {
            let error = anyhow::anyhow!("Tool {name} requires the host to start with --allow-exec");
            return Ok(CallToolResult::structured_error(api::error_json(&error)).into());
        }
        let args = request.arguments.unwrap_or_default();
        let op = match name {
            "workspace_doctor" => {
                parse::<Empty>(args)?;
                Operation::Doctor
            }
            "workspace_list" => {
                parse::<Empty>(args)?;
                Operation::List
            }
            "workspace_spawn" => Operation::Spawn(parse(args)?),
            "workspace_status" => Operation::Status(parse(args)?),
            "workspace_heartbeat" => Operation::Heartbeat(parse(args)?),
            "workspace_env" => Operation::Env(parse(args)?),
            "workspace_exec" => Operation::Exec(parse(args)?),
            "workspace_add_dependency" => Operation::AddDep(parse(args)?),
            "workspace_layer" => Operation::Layer(parse(args)?),
            "workspace_land_and_wipe" => {
                let land: Land = parse(args)?;
                if land.reconcile && !self.allow_exec {
                    let error = anyhow::anyhow!(
                        "Lockfile reconciliation requires the host to start with --allow-exec"
                    );
                    return Ok(CallToolResult::structured_error(api::error_json(&error)).into());
                }
                Operation::Land(land)
            }
            "workspace_discard" => Operation::Drop(parse(args)?),
            "workspace_gc" => Operation::Gc(parse(args)?),
            _ => {
                return Err(ErrorData::invalid_params(
                    format!("Unknown tool: {name}"),
                    None,
                ));
            }
        };
        let repo = self.repo.clone();
        let result = tokio::task::spawn_blocking(move || crate::engine::run(&repo, op))
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

/// Serve MCP over stdin/stdout until the client closes the connection.
pub fn serve(repo: PathBuf, allow_exec: bool) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            Server { repo, allow_exec }
                .serve(rmcp::transport::stdio())
                .await?
                .waiting()
                .await?;
            Ok(())
        })
}
