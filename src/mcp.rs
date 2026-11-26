//! MCP stdio adapter for grafts.
use std::{
    future::Future,
    path::PathBuf,
    pin::pin,
    time::{Duration, Instant},
};

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

use crate::api::{
    self, AddDep, DropWorkspace, Exec, Land, Name, Owned, Push, TurnRelease, TurnTake,
};
use crate::registry;
use crate::workspace::{self, Command, Pushing, Spawn, hook};

const MAX_REQUEST_BYTES: usize = 256 * 1024;

#[derive(Clone)]
struct Server {
    repo: PathBuf,
    allow_exec: bool,
    allow_push: bool,
    /// The lease of every turn this server's client holds. The server is
    /// the holder's process, so its turns end when the client disconnects.
    turn_lease: String,
}

impl Server {
    /// A session's `graft_turn_take` as a `turn take` by this server, with
    /// grafts named relative to the repository.
    fn turn_take(&self, take: api::SessionTurnTake) -> TurnTake {
        TurnTake {
            resource: take.resource,
            holder: take.holder,
            lease: self.turn_lease.clone(),
            pid: Some(std::process::id()),
            grafts: take
                .grafts
                .into_iter()
                .map(|graft| self.repo.join(graft))
                .collect(),
            wait: take.wait,
            ttl: take.ttl,
        }
    }
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
    serde_json::from_value(Value::Object(args)).map_err(invalid)
}

fn invalid(error: impl std::fmt::Display) -> ErrorData {
    ErrorData::invalid_params(format!("Invalid tool arguments: {error}"), None)
}

impl Server {
    /// Run one operation off the async thread.
    async fn run(&self, op: Command) -> Result<anyhow::Result<Value>, ErrorData> {
        let repo = self.repo.clone();
        let (hooks, pushing) = (self.hooks(), self.pushing());
        blocking(move || workspace::run_with(&repo, op, hooks, pushing)).await
    }

    /// Like graft_push, pushing through a remote a graft gets from the
    /// project's configuration needs the host to allow it.
    fn pushing(&self) -> Pushing {
        if self.allow_push {
            Pushing::Allowed
        } else {
            Pushing::Disabled
        }
    }

    /// A hook is a command the project's configuration runs. Like graft_exec,
    /// it runs for an agent only when the host allows running commands.
    fn hooks(&self) -> hook::Policy {
        if self.allow_exec {
            hook::Policy::Run
        } else {
            hook::Policy::Skip
        }
    }
}

/// Run `work` off the async thread.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ErrorData> {
    tokio::task::spawn_blocking(work).await.map_err(|e| {
        ErrorData::internal_error(format!("Graft operation failed to join: {e}"), None)
    })
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        let instructions = if self.allow_exec {
            "Running commands and installing dependencies in grafts is enabled by the host. Hooks the project's .graftconfig and .graftconfig.local define run when a graft is added, merged, removed, pruned or restored; the hooks array of a result reports them. Hooks that come with the repository run only once the user has trusted them in a terminal; until then those calls fail with hooks_untrusted, which you report to the user and must not try to get past."
        } else {
            "Running commands and installing dependencies in grafts is disabled; the host must start with --allow-exec to enable it. Hooks the project's .graftconfig and .graftconfig.local define are not run either; the hooks array of a result reports them as skipped."
        };
        let publishing = if self.allow_push {
            "Pushing a graft's commits to a remote with graft_push is enabled by the host; use it only when the task asks for the work to be published. A remote the remotes array of a graft_add or graft_restore result reports also takes a plain git push from the graft, which graft_push's check against the remote's default branch does not cover: push to that branch only when the task says so."
        } else {
            "Pushing a graft's commits to a remote is disabled; the host must start with --allow-push to enable it. A remote the remotes array of a graft_add or graft_restore result reports can fetch, but Git refuses to push through it."
        };
        let remotes = "The project's .graftconfig and .graftconfig.local can name remotes of the primary that every new graft gets; the remotes array of a graft_add or graft_restore result reports each. Remotes that come with the repository are set up only once the user has trusted them in a terminal; until then graft_add and graft_restore leave them out and report them as skipped with reason untrusted, which you report to the user if the task needs them and must not try to get past.";
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(format!(
                "{instructions} {publishing} {remotes} A graft is an independent copy of the repository for one task. Call graft_add, work inside the returned path, then graft_merge to merge it into a branch. Before using something every graft shares, such as a development server on a fixed port, call graft_turn_take under one holder name for all of your work, and graft_turn_release when done. A graft holds only tracked files: untracked and ignored files such as .env, local config or build output stay behind unless you copy them. Decide what the task needs: call graft_local_files to see them, then pass include to graft_add with .gitignore-style patterns for just the ones to bring. Prefer naming them: [\"*\"] copies all of them, including build output and directories such as target/ and .venv/, which can be large. Omitting include uses the include patterns of the project's .graftconfig and .graftconfig.local, and copies nothing if they set none. For ordinary work in a trusted clone, omit history from graft_add to honor the project setting (default: local). Choose isolated when the graft should only receive the selected commit's reachable history. Each newline-delimited message may contain up to {MAX_REQUEST_BYTES} bytes. Larger messages are discarded."
            ))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut tools = vec![
            tool::<Spawn>(
                "graft_add",
                "Create a graft: an independent copy of the repository with its own branch. Returns the graft's path and lease; keep the lease, it authorizes every later call. Omit history for normal work (the project setting defaults to local, which reuses the trusted clone's Git objects). Choose isolated to keep unrelated objects such as staged blobs or other branches' commits out. Untracked and ignored files, such as .env, are not copied unless include names them (see graft_local_files) or the include list of the project's .graftconfig or .graftconfig.local does. Name what the task needs; [\"*\"] copies all of them, including large build and environment directories.",
                false,
                false,
            ),
            tool::<Empty>(
                "graft_list",
                "List active and recoverable grafts.",
                true,
                false,
            ),
            tool::<Empty>(
                "graft_local_files",
                "List the checkout's untracked and ignored files and directories, such as .env, local config and build output. A new graft holds none of them; pass the ones its task needs to graft_add as include patterns. Also reports, as defaults, the include patterns the project's .graftconfig and .graftconfig.local set, which graft_add uses when include is omitted.",
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
        tools.push(tool::<Land>(
            "graft_merge",
            "Merge a graft's commits into a local branch, then archive the graft. Set undo to reverse an earlier merge instead: it appends a commit only if the branch is still at the merged commit. Lockfile reconciliation needs host --allow-exec.",
            false,
            true,
        ));
        if self.allow_push {
            // Marked destructive: a push changes a remote that others share,
            // and nothing here can take it back.
            tools.push(tool::<Push>(
                "graft_push",
                "Push a graft's commits to a branch on a remote of the primary repository, for example to open a pull request. Uses the primary's remote configuration and credentials, never forces, updates the remote's default branch only when to names it, and leaves the graft active; uncommitted work is not pushed. Use it only when the task asks for the work to be published. Needs host --allow-push.",
                false,
                true,
            ));
        }
        tools.extend([
            tool::<DropWorkspace>(
                "graft_remove",
                "Archive a graft and its files. Unless discard is set, refuses and names what it found when the graft has uncommitted, untracked or ignored files, unmerged commits, a stash, or extra branches or tags; keep the recovery lease to reopen it.",
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
            tool::<api::SessionTurnTake>(
                "graft_turn_take",
                "Take a turn on something every graft shares and cannot have a copy of, such as a development server on a fixed port or a local database. resource is a name the project chooses. holder is the name to hold it under, such as your task's: one name for all of this work, even across grafts of several repositories. List those grafts' paths in grafts; once none of them is active the turn is lost. granted true means you hold the turn. Otherwise you are queued and position is your place in line, 1 being next: call again within 300 seconds to keep the place, or set wait to keep asking, for less time than your client allows a tool call. Call again while you hold the turn to renew it before expires_at. The turn also ends when this MCP session does. Graft does nothing to the resource itself, but the project can define post-take hooks, which run when you are newly granted the turn or name different grafts; the result then has a hooks array, each entry with its status and output, and skipped entries when the host was not started with --allow-exec. If a hook fails, the call fails with hook_failed and you do not hold the turn: read details.hooks, tell the user, and do not use the resource. A turn is a promise between cooperating agents. Give it up with graft_turn_release as soon as you finish.",
                false,
                false,
            ),
            tool::<api::SessionTurnRelease>(
                "graft_turn_release",
                "Give up your turn on a shared resource, or leave its queue, under the holder name you took it with. next names the holder now first in line; it gets the turn when it next calls graft_turn_take.",
                false,
                false,
            ),
            tool::<Empty>(
                "graft_turn_list",
                "List the shared resources that have a holder or a queue: who holds each turn, the grafts the holder works in, when the turn expires and who waits, in order.",
                true,
                false,
            ),
        ]);
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
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
        if !self.allow_push && name == "graft_push" {
            return refuse(format!(
                "Tool {name} requires the host to start with --allow-push"
            ));
        }
        let args = request.arguments.unwrap_or_default();
        let op = match name {
            "graft_list" => parse::<Empty>(args).map(|_| Command::List)?,
            "graft_local_files" => parse::<Empty>(args).map(|_| Command::LocalFiles)?,
            "graft_add" => {
                Command::Spawn(Spawn::from_mcp(args, !self.allow_exec).map_err(invalid)?)
            }
            "graft_status" => Command::Status(parse(args)?),
            "graft_renew" => Command::Heartbeat(parse(args)?),
            "graft_exec" => Command::Exec(parse(args)?),
            "graft_add_dependency" => Command::AddDep(parse(args)?),
            "graft_restore" => Command::Restore(parse(args)?),
            "graft_merge" => {
                let explicit_into = args.contains_key("into");
                let land: Land = parse(args)?;
                land.check_undo(explicit_into).map_err(invalid)?;
                if land.reconcile && !self.allow_exec {
                    return refuse(
                        "Lockfile reconciliation requires the host to start with --allow-exec"
                            .into(),
                    );
                }
                Command::land(land)
            }
            "graft_push" => Command::Push(parse(args)?),
            "graft_remove" => Command::Drop(parse(args)?),
            "graft_turn_take" => {
                let take = self.turn_take(parse(args)?);
                take.check().map_err(invalid)?;
                Command::TurnTake(take)
            }
            "graft_turn_release" => {
                let release: api::SessionTurnRelease = parse(args)?;
                Command::TurnRelease(TurnRelease {
                    resource: release.resource,
                    holder: release.holder,
                    lease: self.turn_lease.clone(),
                })
            }
            "graft_turn_list" => parse::<Empty>(args).map(|_| Command::TurnList)?,
            _ => {
                return Err(ErrorData::invalid_params(
                    format!("Unknown tool: {name}"),
                    None,
                ));
            }
        };
        let result = match op {
            Command::TurnTake(take) => {
                // The Git version and the project are found once, not on
                // every ask.
                let repo = self.repo.clone();
                let policy = self.hooks();
                let command = Command::TurnTake(take.clone());
                match blocking(move || workspace::find(&repo, &command)).await? {
                    Err(error) => Err(error),
                    Ok(found) => {
                        let ask = |take| {
                            let (repo, found) = (self.repo.clone(), found.clone());
                            blocking(move || {
                                workspace::run_found(
                                    &repo,
                                    found,
                                    Command::TurnTake(take),
                                    policy,
                                    Pushing::Disabled,
                                )
                            })
                        };
                        take_turn(
                            take,
                            || context.ct.is_cancelled(),
                            context.ct.cancelled(),
                            ask,
                            |release| self.run(Command::TurnRelease(release)),
                        )
                        .await?
                    }
                }
            }
            op => self.run(op).await?,
        };
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

type Reply = Result<anyhow::Result<Value>, ErrorData>;

/// `graft_turn_take`. A waiting take asks from here, one ask at a time, so
/// it stops asking when the client cancels the request or
/// disconnects instead of taking a turn nobody will use. `cancelled`
/// completes when the request is cancelled, and `is_cancelled` reports
/// whether it has been.
///
/// A turn granted after the request was cancelled has no reader: the
/// response is dropped. This call then releases it, unless the graft held
/// the turn before the call and the grant only renewed it.
async fn take_turn<Ask, Asked, Release, Released>(
    mut take: TurnTake,
    is_cancelled: impl Fn() -> bool,
    cancelled: impl Future<Output = ()>,
    mut ask: Ask,
    release: Release,
) -> Reply
where
    Ask: FnMut(TurnTake) -> Asked,
    Asked: Future<Output = Reply>,
    Release: FnOnce(TurnRelease) -> Released,
    Released: Future<Output = Reply>,
{
    let asked = registry::now();
    let deadline = Instant::now() + Duration::from_secs(take.wait);
    take.wait = 0;
    let mut cancelled = pin!(cancelled);
    loop {
        let reply = ask(take.clone()).await?;
        if let Ok(granted) = &reply
            && granted["granted"] == true
            && is_cancelled()
        {
            // A new turn starts during this call; a renewed one started before.
            if granted["since"]
                .as_u64()
                .is_some_and(|since| since >= asked)
            {
                // If the release fails, the turn still runs out after its ttl.
                let _ = release(TurnRelease {
                    resource: take.resource,
                    holder: take.holder,
                    lease: take.lease,
                })
                .await;
            }
            return Ok(reply);
        }
        let pause = reply
            .as_ref()
            .ok()
            .and_then(|reply| workspace::turn_pause(reply, deadline));
        let Some(pause) = pause else {
            return Ok(reply);
        };
        if tokio::time::timeout(pause, cancelled.as_mut())
            .await
            .is_ok()
        {
            return Ok(reply);
        }
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
pub fn serve(repo: PathBuf, allow_exec: bool, allow_push: bool) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            let (stdin, stdout) = rmcp::transport::stdio();
            Server {
                repo,
                allow_exec,
                allow_push,
                turn_lease: uuid::Uuid::new_v4().to_string(),
            }
            .serve((bounded_input(stdin), stdout))
            .await?
            .waiting()
            .await?;
            Ok(())
        })
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::future::{pending, ready};

    use serde_json::json;

    use super::*;

    fn take(wait: u64) -> TurnTake {
        TurnTake {
            resource: "web".into(),
            holder: "task".into(),
            lease: "lease".into(),
            pid: None,
            grafts: Vec::new(),
            wait,
            ttl: None,
        }
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime builds")
            .block_on(future)
    }

    /// Runs `take_turn` against `replies`, one per ask, and returns its
    /// reply, the number of asks and the releases it sent.
    fn run(
        wait: u64,
        replies: Vec<Value>,
        is_cancelled: impl Fn(usize) -> bool,
        cancelled: impl Future<Output = ()>,
    ) -> (Value, usize, Vec<TurnRelease>) {
        let asks = Cell::new(0);
        let releases = RefCell::new(Vec::new());
        let reply = block_on(take_turn(
            take(wait),
            || is_cancelled(asks.get()),
            cancelled,
            |take| {
                assert_eq!(take.wait, 0, "each ask is a single ask");
                let reply = replies[asks.get()].clone();
                asks.set(asks.get() + 1);
                ready(Ok(Ok(reply)))
            },
            |release| {
                releases.borrow_mut().push(release);
                ready(Ok(Ok(json!({"released": true}))))
            },
        ));
        let reply = reply.expect("joins").expect("succeeds");
        (reply, asks.get(), releases.into_inner())
    }

    fn queued() -> Value {
        json!({"granted": false, "position": 1})
    }

    fn granted(since: u64) -> Value {
        json!({"granted": true, "since": since})
    }

    #[test]
    fn a_take_that_is_not_cancelled_returns_the_grant() {
        let (reply, asks, releases) = run(60, vec![granted(registry::now())], |_| false, pending());
        assert_eq!(reply["granted"], true);
        assert_eq!(asks, 1);
        assert!(releases.is_empty());
    }

    #[test]
    fn a_wait_cancelled_during_the_pause_stops_asking() {
        let (reply, asks, releases) = run(60, vec![queued(), granted(0)], |_| false, ready(()));
        assert_eq!(reply, queued());
        assert_eq!(asks, 1);
        assert!(releases.is_empty());
    }

    #[test]
    fn a_turn_granted_after_cancellation_is_released() {
        let now = registry::now();
        for wait in [0, 60] {
            let (reply, asks, releases) = run(wait, vec![granted(now)], |_| true, pending());
            assert_eq!(reply["granted"], true);
            assert_eq!(asks, 1);
            let [release] = &releases[..] else {
                panic!("expected one release, got {}", releases.len());
            };
            assert_eq!(
                (&*release.holder, &*release.resource, &*release.lease),
                ("task", "web", "lease")
            );
        }
    }

    #[test]
    fn a_wait_granted_after_cancellation_is_released() {
        // Cancelled while the second ask runs, which grants the turn.
        let now = registry::now();
        let (reply, asks, releases) =
            run(1, vec![queued(), granted(now)], |asks| asks == 2, pending());
        assert_eq!(reply["granted"], true);
        assert_eq!(asks, 2);
        assert_eq!(releases.len(), 1);
    }

    #[test]
    fn a_renewal_after_cancellation_keeps_the_turn() {
        let before = registry::now().saturating_sub(60);
        let (reply, asks, releases) = run(60, vec![granted(before)], |_| true, pending());
        assert_eq!(reply["granted"], true);
        assert_eq!(asks, 1);
        assert!(releases.is_empty());
    }
}
