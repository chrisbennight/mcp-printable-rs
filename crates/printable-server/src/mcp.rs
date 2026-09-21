//! The Printable MCP request handler (house `ServerHandler` style).
//!
//! Tool dispatch is table-driven ([`crate::tools`]); the
//! `list_tools_payload`/`invoke_tool` shims are public and `RequestContext`-free
//! so tests can drive them directly. Resources are served from
//! [`crate::resources`].

use std::borrow::Cow;
use std::sync::Arc;

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, CustomRequest,
        CustomResult, Implementation, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
        ProtocolVersion, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult,
        Resource, ResourceContents, ServerCapabilities, ServerInfo, Tool,
    },
    service::RequestContext,
};
use serde_json::{Map, Value};

use printable_blender::BlenderClient;
use printable_scad::ScadRunner;
use printable_workspace::Workspace;

use crate::{
    config::Settings,
    error::ToolError,
    file_ingest::{
        AuthorizeUploadParams, INGEST_TOOL, IncomingFiles, IngestParams, TransferStatusParams,
    },
    file_transfer::{
        AuthorizeDownloadParams, PUBLISH_TOOL, PublishParams, PublishedFiles,
        client_supports_http_download,
    },
    health::{ReadinessChecks, ReadinessResponse, ReadinessWork, SERVER_NAME},
    jobs::JobRegistry,
    resources, tools,
    upload::UploadRegistry,
};

const SERVER_INSTRUCTIONS: &str = "Printable provides fifteen workflow tools for Blender, OpenSCAD, and CadQuery modeling, native observation, manufacturing evidence, rendering, and confined artifacts. Combined tools select an action and typed params. Use inspect for bounded state, blender_execute for general Python, and view for visual feedback. Checkpoint before risky edits; use expected_scene to reject stale state. Never retry a mutation after an uncertain timeout: wait for healthy status, then inspect or restore. Use immutable checkpoints and job for long rendering; poll compact progress and request detail only for full evidence. Uploads and file publication belong to artifact. Paths are workspace-relative; large data belongs in artifacts. Images do not certify dimensions, wall thickness, or motion clearance. Read printable://modeling/blender-v1, printable://design/product-v1, and printable://render/product-v1 for task-specific guidance.";

/// The MCP request handler. The backend handles are `Arc`-shared so the server
/// stays `Clone` (the transport builds one per session); the underlying
/// `Workspace` and `BlenderClient` are not themselves `Clone`.
#[derive(Clone)]
pub struct PrintableServer {
    workspace: Arc<Workspace>,
    /// Chunked-upload registry, shared across sessions so its concurrency and
    /// size bounds protect the whole process (see [`crate::upload`]).
    uploads: Arc<UploadRegistry>,
    blender: Arc<BlenderClient>,
    settings: Arc<Settings>,
    scad: Arc<ScadRunner>,
    jobs: Arc<JobRegistry>,
    published_files: Arc<PublishedFiles>,
    incoming_files: Arc<IncomingFiles>,
}

impl PrintableServer {
    pub fn new(
        workspace: Arc<Workspace>,
        blender: Arc<BlenderClient>,
        settings: Arc<Settings>,
    ) -> Self {
        let scad = Arc::new(ScadRunner::discover(
            settings.openscad_bin.clone(),
            settings.scad_concurrency,
        ));
        let worker = settings.render_worker_host.as_ref().map(|host| {
            Arc::new(BlenderClient::new(
                host.clone(),
                settings.render_worker_port,
                printable_blender::ClientOptions::default(),
            ))
        });
        let jobs = Arc::new(JobRegistry::new_with_render_worker(
            Arc::clone(&workspace),
            Arc::clone(&blender),
            settings.ffmpeg_bin.clone(),
            settings.render_job_queue_depth,
            settings.geometry_worker_bin.clone(),
            settings.geometry_worker_memory_bytes,
            worker,
        ));
        Self {
            published_files: Arc::new(PublishedFiles::new(Arc::clone(&workspace))),
            incoming_files: Arc::new(IncomingFiles::new(
                Arc::clone(&workspace),
                settings.file_upload_max_bytes,
            )),
            workspace,
            uploads: Arc::new(UploadRegistry::new()),
            blender,
            settings,
            scad,
            jobs,
        }
    }

    pub(crate) fn incoming_files(&self) -> Arc<IncomingFiles> {
        Arc::clone(&self.incoming_files)
    }

    pub(crate) fn published_files(&self) -> Arc<PublishedFiles> {
        Arc::clone(&self.published_files)
    }

    fn to_rmcp_tool(meta: &tools::ToolDef) -> Tool {
        let tool = Tool::new(
            Cow::Borrowed(meta.name),
            Cow::Borrowed(meta.description),
            (meta.schema)(),
        )
        .annotate((meta.annotations)());
        if tools::workflows::lookup(meta.name).is_some() {
            tool.with_raw_output_schema(tools::output::schema(meta.name))
        } else {
            tool
        }
    }

    /// Build the `ListToolsResult`. Kept out of the `ServerHandler` impl so tests
    /// can drive it without a `RequestContext`.
    pub fn list_tools_payload(&self) -> ListToolsResult {
        self.workflow_tools_payload()
    }

    pub fn workflow_tools_payload(&self) -> ListToolsResult {
        ListToolsResult::with_all_items(
            tools::workflows::TOOLS
                .iter()
                .map(Self::to_rmcp_tool)
                .collect(),
        )
    }

    /// Resolve a tool name and dispatch. An unknown name is a JSON-RPC
    /// method-not-found; a tool-level failure is a successful result flagged
    /// `is_error` carrying a machine code + message.
    pub async fn invoke_tool(
        &self,
        params: CallToolRequestParams,
    ) -> Result<CallToolResult, McpError> {
        if tools::workflows::lookup(&params.name).is_none() {
            return Err(McpError::method_not_found::<
                rmcp::model::CallToolRequestMethod,
            >());
        }
        let args = params
            .arguments
            .map(Value::Object)
            .unwrap_or(Value::Object(Map::new()));

        let (operation, args, projection) = match tools::workflows::resolve(&params.name, args) {
            Ok(call) => (call.name, call.arguments, call.response),
            Err(error) => return Ok(error_result(&params.name, &error)),
        };

        if operation == INGEST_TOOL || operation == "printable_workspace_transfer_status" {
            let result: Result<Value, ToolError> = async {
                if operation == INGEST_TOOL {
                    let request: IngestParams = serde_json::from_value(args)
                        .map_err(|e| ToolError::Validation(e.to_string()))?;
                    Ok(
                        serde_json::to_value(self.incoming_files.ingest(request).await?)
                            .expect("artifact serializes"),
                    )
                } else {
                    let request: TransferStatusParams = serde_json::from_value(args)
                        .map_err(|e| ToolError::Validation(e.to_string()))?;
                    self.incoming_files.status(&request.uri).await
                }
            }
            .await;
            return Ok(match result {
                Ok(value) => {
                    let mut result =
                        CallToolResult::success(vec![ContentBlock::text(value.to_string())]);
                    result.structured_content = Some(value);
                    result
                }
                Err(error) => error_result(&params.name, &error),
            });
        }

        if operation == "printable_printer" || operation == "printable_print" {
            let result: Result<Value, ToolError> = async {
                let service = self.settings.printers.as_ref().ok_or_else(|| {
                    ToolError::Validation("printer integration is not configured".into())
                })?;
                if operation == "printable_printer" {
                    service
                        .observe(
                            serde_json::from_value(args)
                                .map_err(|error| ToolError::Validation(error.to_string()))?,
                            &self.workspace,
                        )
                        .await
                } else {
                    service
                        .control(
                            serde_json::from_value(args)
                                .map_err(|error| ToolError::Validation(error.to_string()))?,
                            &self.workspace,
                        )
                        .await
                }
            }
            .await;
            return Ok(match result {
                Ok(value) => {
                    let mut result =
                        CallToolResult::success(vec![ContentBlock::text(value.to_string())]);
                    result.structured_content = Some(value);
                    result
                }
                Err(error) => error_result(&params.name, &error),
            });
        }

        if operation == "printable_project" {
            let workspace = Arc::clone(&self.workspace);
            let result = tokio::task::spawn_blocking(move || {
                let request = serde_json::from_value(args)
                    .map_err(|error| ToolError::Validation(error.to_string()))?;
                crate::projects::dispatch(&workspace, request)
            })
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
            return Ok(match result {
                Ok(value) => {
                    let mut result =
                        CallToolResult::success(vec![ContentBlock::text(value.to_string())]);
                    result.structured_content = Some(value);
                    result
                }
                Err(error) => error_result(&params.name, &error),
            });
        }

        if operation == "printable_cad_build" {
            let result = match serde_json::from_value(args) {
                Ok(request) => {
                    crate::cad::forward(
                        &self.workspace,
                        self.settings.cad_endpoint.as_deref(),
                        request,
                    )
                    .await
                }
                Err(error) => Err(ToolError::Validation(error.to_string())),
            };
            return Ok(match result {
                Ok(value) => {
                    let mut result =
                        CallToolResult::success(vec![ContentBlock::text(value.to_string())]);
                    result.structured_content = Some(value);
                    result
                }
                Err(error) => error_result(&params.name, &error),
            });
        }

        if operation == PUBLISH_TOOL {
            let publish: PublishParams = match serde_json::from_value(args) {
                Ok(params) => params,
                Err(error) => {
                    return Ok(error_result(
                        &params.name,
                        &ToolError::Validation(error.to_string()),
                    ));
                }
            };
            return match self.published_files.publish(publish).await {
                Ok(file) => {
                    let value = serde_json::json!({ "file": file });
                    let mut result =
                        CallToolResult::success(vec![ContentBlock::text(value.to_string())]);
                    result.structured_content = Some(value);
                    Ok(result)
                }
                Err(error) => Ok(error_result(&params.name, &error)),
            };
        }

        match tools::dispatch_with_content(
            &self.workspace,
            &self.uploads,
            &self.blender,
            &self.settings,
            (&self.scad, Some(&self.jobs)),
            operation,
            args,
        )
        .await
        {
            Ok(output) => {
                let (value, image) = if operation == "printable_render_preview" {
                    match tools::prepare_inline_png_content(
                        Arc::clone(&self.workspace),
                        output.value,
                    )
                    .await
                    {
                        Ok(prepared) => prepared,
                        Err(err) => return Ok(error_result(&params.name, &err)),
                    }
                } else {
                    (output.value, output.inline_png_base64)
                };
                let value = if operation == "printable_workspace_list" {
                    serde_json::json!({ "entries": value })
                } else {
                    projection.apply(value)
                };
                let mut content = vec![ContentBlock::text(value.to_string())];
                if let Some(data_base64) = image {
                    content.push(ContentBlock::image(data_base64, "image/png"));
                }
                let mut result = CallToolResult::success(content);
                result.structured_content = Some(value);
                Ok(result)
            }
            Err(err) => Ok(error_result(&params.name, &err)),
        }
    }

    pub(crate) async fn readiness(&self) -> ReadinessResponse {
        let blender_probe = self.blender.try_send_value(
            "bridge_status",
            printable_blender::Params::new(),
            printable_blender::Deadline::new(std::time::Duration::from_secs(5)),
        );
        let jobs = self.jobs.health();
        let openscad = self.scad.ready();
        let (blender_probe, jobs, openscad) = tokio::join!(blender_probe, jobs, openscad);
        let (blender, blender_busy) = match blender_probe {
            Ok(Some(_)) => (true, false),
            Ok(None) => (true, true),
            Err(_) => (false, false),
        };
        aggregate_readiness(
            blender,
            blender_busy,
            self.workspace.ready(),
            openscad,
            &jobs,
        )
    }
}

fn aggregate_readiness(
    blender: bool,
    blender_busy: bool,
    workspace: bool,
    openscad: bool,
    jobs: &Value,
) -> ReadinessResponse {
    let ffmpeg = jobs["encoder"]["available"].as_bool().unwrap_or(false);
    let durable_recovery = jobs["recovery_integrity"]["status"].as_str() == Some("ok");
    let queued = jobs["queued"].as_u64().unwrap_or(0) > 0;
    let running = jobs["running"].as_u64().unwrap_or(0) > 0;

    ReadinessResponse::new(
        ReadinessChecks {
            blender,
            render_worker: jobs["worker"]["available"].as_bool().unwrap_or(false),
            workspace,
            openscad,
            ffmpeg,
            durable_recovery,
        },
        ReadinessWork {
            queued,
            running: running || blender_busy,
        },
    )
}

impl ServerHandler for PrintableServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::V_2024_11_05;
        let capabilities = ServerCapabilities::builder().enable_tools();
        info.capabilities = if resources::RESOURCES.is_empty() {
            capabilities.build()
        } else {
            capabilities.enable_resources().build()
        };
        // `from_build_env` captures rmcp's own package identity (name and
        // version), so override both: the client must see `printable_blender` at
        // this server's crate version, not the transport dependency's.
        info.server_info = Implementation::from_build_env();
        info.server_info.name = SERVER_NAME.to_string();
        info.server_info.version = env!("CARGO_PKG_VERSION").to_string();
        info.instructions = Some(SERVER_INSTRUCTIONS.to_string());
        info
    }

    async fn list_tools(
        &self,
        _params: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(self.list_tools_payload())
    }

    async fn call_tool(
        &self,
        params: CallToolRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        self.invoke_tool(params).await.map(CallToolResponse::from)
    }

    async fn on_custom_request(
        &self,
        request: CustomRequest,
        context: RequestContext<RoleServer>,
    ) -> Result<CustomResult, McpError> {
        if request.method == "files/authorizeUpload" {
            if !crate::file_transfer::client_supports_http_transfer(&context.meta, "upload") {
                return Err(McpError::invalid_params(
                    "file authorization requires declared HTTP upload support",
                    None,
                ));
            }
            let params = request
                .params_as::<AuthorizeUploadParams>()
                .map_err(|e| McpError::invalid_params(e.to_string(), None))?
                .ok_or_else(|| McpError::invalid_params("missing upload parameters", None))?;
            let base_url = transfer_base_url(&context, self.settings.download_base_url.as_ref())?;
            return self
                .incoming_files
                .authorize(params, &base_url)
                .map(CustomResult::new)
                .map_err(|e| McpError::invalid_params(e.to_string(), None));
        }
        if request.method != "files/authorizeDownload" {
            return Err(McpError::new(
                rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                request.method,
                None,
            ));
        }
        let params = request
            .params_as::<AuthorizeDownloadParams>()
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?
            .ok_or_else(|| {
                McpError::invalid_params("missing file authorization parameters", None)
            })?;
        if !client_supports_http_download(&context.meta) {
            return Err(McpError::invalid_params(
                "file authorization requires declared HTTP download support",
                None,
            ));
        }
        let base_url = transfer_base_url(&context, self.settings.download_base_url.as_ref())?;
        let result = self
            .published_files
            .authorize_download(params, &base_url)
            .map_err(|message| McpError::invalid_params(message, None))?;
        Ok(CustomResult::new(result))
    }

    async fn list_resources(
        &self,
        _params: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let mut resources: Vec<Resource> = resources::RESOURCES
            .iter()
            .map(|r| {
                Resource::new(r.uri, r.name)
                    .with_description(r.description)
                    .with_mime_type(r.mime_type)
            })
            .collect();
        resources.push(Resource::new(resources::contracts::ROOT, "Printable operation contracts")
            .with_description("Compact tool/action index; read printable://contracts/{tool}/{action} for a selected schema, without loading every engine's contract.")
            .with_mime_type("application/json"));
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        params: ReadResourceRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        if let Some(contract) = resources::contracts::read(&params.uri) {
            return Ok(ReadResourceResponse::from(ReadResourceResult::new(vec![
                ResourceContents::text(contract.to_string(), params.uri)
                    .with_mime_type("application/json"),
            ])));
        }
        match resources::RESOURCES.iter().find(|r| r.uri == params.uri) {
            Some(resource) => Ok(ReadResourceResponse::from(ReadResourceResult::new(vec![
                ResourceContents::text(resource.body, params.uri)
                    .with_mime_type(resource.mime_type),
            ]))),
            None => Err(McpError::method_not_found::<
                rmcp::model::ReadResourceRequestMethod,
            >()),
        }
    }
}

fn transfer_base_url(
    context: &RequestContext<RoleServer>,
    configured: Option<&reqwest::Url>,
) -> Result<reqwest::Url, McpError> {
    let authority = context
        .extensions
        .get::<axum::http::request::Parts>()
        .and_then(|parts| parts.headers.get(axum::http::header::HOST))
        .and_then(|host| host.to_str().ok())
        .filter(|host| !host.contains('@'))
        .ok_or_else(|| McpError::invalid_params("missing HTTP host authority", None))?;
    match configured {
        Some(base) => Ok(base.clone()),
        None => reqwest::Url::parse(&format!("http://{authority}/"))
            .map_err(|_| McpError::invalid_params("invalid HTTP host authority", None)),
    }
}

/// Render a [`ToolError`] into the MCP error envelope: a successful result
/// flagged `is_error`, carrying a machine code + message the agent can act on.
fn error_result(tool: &str, err: &ToolError) -> CallToolResult {
    let mut payload = serde_json::json!({
        "tool": tool,
        "error": { "code": err.code(), "message": err.to_string() },
    });
    if let ToolError::Blender(error) = err
        && let Some(state) = error.scene_state()
    {
        payload["error"]["scene_state"] = serde_json::json!(state);
    }
    if let ToolError::Printer(bambuddy_api::ApiError::Rejected(details)) = err {
        payload["error"]["details"] = serde_json::json!(details);
        payload["error"]["outcome"] = serde_json::json!("rejected");
    }
    let text = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    result.structured_content = Some(payload);
    result.is_error = Some(true);
    result
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::aggregate_readiness;

    #[test]
    fn active_job_checkpoint_guard_is_healthy_busy_work() {
        let busy = aggregate_readiness(
            true,
            true,
            true,
            true,
            &json!({
                "queued": 0,
                "running": 1,
                "recovery_fenced": true,
                "recovery_integrity": {"status": "ok", "issues": []},
                "encoder": {"available": true},
                "worker": {"available": true}
            }),
        );

        assert_eq!(busy.status, "busy");
        assert_eq!(busy.codes, ["work_in_progress"]);
        assert!(busy.checks.durable_recovery);
        assert!(busy.work.running);

        let blocked = aggregate_readiness(
            true,
            false,
            true,
            true,
            &json!({
                "queued": 0,
                "running": 1,
                "recovery_fenced": true,
                "recovery_integrity": {
                    "status": "blocked",
                    "issues": ["job metadata index is invalid"]
                },
                "encoder": {"available": true},
                "worker": {"available": true}
            }),
        );

        assert_eq!(blocked.status, "blocked");
        assert_eq!(blocked.codes, ["durable_recovery_fenced"]);
        assert!(!blocked.checks.durable_recovery);
        assert!(blocked.work.running);
    }
}
