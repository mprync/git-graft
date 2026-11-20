//! MCP stdio adapter for grafts.
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

use crate::api::{self, AddDep, DropWorkspace, Exec, Land, Name, Owned};
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
            "Running commands and installing dependencies in grafts is enabled by the host."
        } else {
            "Running commands and installing dependencies in grafts is disabled; the host must start with --allow-exec to enable it."
        };
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(format!(
                "{instructions} A graft is an independent copy of the repository for one task. Call graft_new, work inside the returned path, then graft_land to merge it into a branch. For ordinary work in a trusted clone, omit history from graft_new to honor the project setting (default: local). Choose isolated when the graft should only receive the selected commit's reachable history. Each newline-delimited message may contain up to {MAX_REQUEST_BYTES} bytes. Larger messages are discarded."
            ))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut tools = vec![
            tool::<Spawn>(
                "graft_new",
                "Create a graft: an independent copy of the repository with its own branch. Returns the graft's path and lease; keep the lease, it authorizes every later call. Omit history for normal work (the project setting defaults to local, which reuses the trusted clone's Git objects). Choose isolated to keep unrelated objects such as staged blobs or other branches' commits out.",
                false,
                false,
            ),
            tool::<Empty>(
                "graft_list",
                "List active and recoverable grafts.",
                true,
                false,
            ),
            tool::<Name>(
                "graft_status",
                "Show a graft's HEAD, branch and changed files.",
                true,
                false,
            ),
        ];
        if self.allow_exec {
            tools.push(tool::<Exec>(
                "graft_exec",
                "Run a program inside a graft. A registered package manager matches the program name automatically; pass manager for wrappers. Needs host --allow-exec.",
                false,
                true,
            ));
            tools.push(tool::<AddDep>(
                "graft_add_dependency",
                "Install packages in a graft, or run a locked install when packages is empty. Needs host --allow-exec.",
                false,
                true,
            ));
        }
        tools.extend([
            tool::<Land>(
                "graft_land",
                "Merge a graft's commits into a local branch, then archive the graft. Set undo to reverse an earlier land instead: it appends a commit only if the branch is still at the landed commit. Lockfile reconciliation needs host --allow-exec.",
                false,
                true,
            ),
            tool::<DropWorkspace>(
                "graft_prune",
                "Archive a graft and its files. Refuses if the graft has local work or unlanded commits unless discard is set; keep the recovery lease to reopen it.",
                false,
                true,
            ),
            tool::<Owned>(
                "graft_restore",
                "Reopen an archived graft using its recovery lease.",
                false,
                false,
            ),
            tool::<Owned>(
                "graft_renew",
                "Renew a graft's lease so it is not treated as expired.",
                false,
                false,
            ),
        ]);
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name.as_ref();
        let refuse = |message: String| -> Result<CallToolResponse, ErrorData> {
            let error = anyhow::anyhow!(message);
            Ok(CallToolResult::structured_error(api::error_json(&error)).into())
        };
        if !self.allow_exec && matches!(name, "graft_exec" | "graft_add_dependency") {
            return refuse(format!(
                "Tool {name} requires the host to start with --allow-exec"
            ));
        }
        let args = request.arguments.unwrap_or_default();
        let op = match name {
            "graft_list" => parse::<Empty>(args).map(|_| Command::List)?,
            "graft_new" => {
                Command::Spawn(Spawn::from_mcp(args, !self.allow_exec).map_err(|e| {
                    ErrorData::invalid_params(format!("Invalid tool arguments: {e}"), None)
                })?)
            }
            "graft_status" => Command::Status(parse(args)?),
            "graft_renew" => Command::Heartbeat(parse(args)?),
            "graft_exec" => Command::Exec(parse(args)?),
            "graft_add_dependency" => Command::AddDep(parse(args)?),
            "graft_restore" => Command::Restore(parse(args)?),
            "graft_land" => {
                let explicit_into = args.contains_key("into");
                let land: Land = parse(args)?;
                land.check_undo(explicit_into).map_err(|e| {
                    ErrorData::invalid_params(format!("Invalid tool arguments: {e}"), None)
                })?;
                if land.reconcile && !self.allow_exec {
                    return refuse(
                        "Lockfile reconciliation requires the host to start with --allow-exec"
                            .into(),
                    );
                }
                Command::land(land)
            }
            "graft_prune" => Command::Drop(parse(args)?),
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
                ErrorData::internal_error(format!("Graft operation failed to join: {e}"), None)
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
