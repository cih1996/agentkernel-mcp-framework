use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;
use tower_http::cors::CorsLayer;
use tracing::{debug, error, info, warn};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

const DEFAULT_LOG_DIR_NAME: &str = "临时日志";
const DEFAULT_LOG_LEVEL: &str = "info";
const LOG_PREVIEW_LIMIT: usize = 4000;
const DEFAULT_INIT_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_LIST_TOOLS_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_TOOL_CALL_TIMEOUT_MS: u64 = 300_000;

#[derive(Debug, Error)]
enum FrameworkError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("server not found: {0}")]
    ServerNotFound(String),
    #[error("tool not found: {0}")]
    ToolNotFound(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("json error: {0}")]
    Json(String),
    #[error("mcp error: {0}")]
    Mcp(String),
}

impl From<std::io::Error> for FrameworkError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value.to_string())
    }
}

impl From<serde_json::Error> for FrameworkError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value.to_string())
    }
}

impl IntoResponse for FrameworkError {
    fn into_response(self) -> Response {
        let status = match self {
            FrameworkError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            FrameworkError::ServerNotFound(_) | FrameworkError::ToolNotFound(_) => StatusCode::NOT_FOUND,
            FrameworkError::Io(_) | FrameworkError::Json(_) | FrameworkError::Mcp(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = Json(json!({
            "ok": false,
            "error": self.to_string()
        }));
        (status, body).into_response()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigEnvelope {
    #[serde(default)]
    mcp_servers: HashMap<String, McpServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct McpServerConfig {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolDefinition {
    name: String,
    description: String,
    input_schema: Value,
}

struct ManagedServer {
    name: String,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    request_id: u64,
    tools: Vec<ToolDefinition>,
}

#[derive(Clone)]
struct AppState {
    servers: Arc<Mutex<HashMap<String, Arc<Mutex<ManagedServer>>>>>,
    tool_routes: Arc<Mutex<HashMap<String, ToolRoute>>>,
    timeouts: TimeoutConfig,
}

#[derive(Debug, Clone)]
struct ToolRoute {
    server_name: String,
    remote_tool_name: String,
}

#[derive(Clone)]
struct TimeoutConfig {
    initialize: Duration,
    list_tools: Duration,
    tool_call: Duration,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoadConfigRequest {
    #[serde(default)]
    config: Option<ConfigEnvelope>,
    #[serde(default)]
    config_path: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CallToolRequest {
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ApiResponse<T: Serialize> {
    ok: bool,
    data: T,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FailedServerInfo {
    name: String,
    command: String,
    args: Vec<String>,
    stage: String,
    error: String,
    duration_ms: u128,
}

struct LoggingRuntime {
    log_dir: PathBuf,
    level: String,
    _guard: WorkerGuard,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let logging = init_logging(&args)?;
    let host = arg_value(&args, "--host").unwrap_or_else(|| "127.0.0.1".to_string());
    let port = arg_value(&args, "--port").unwrap_or_else(|| "9528".to_string());
    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    let timeouts = TimeoutConfig {
        initialize: timeout_from_args_env(
            &args,
            "--init-timeout-ms",
            "MCP_FRAMEWORK_INIT_TIMEOUT_MS",
            DEFAULT_INIT_TIMEOUT_MS,
        ),
        list_tools: timeout_from_args_env(
            &args,
            "--list-tools-timeout-ms",
            "MCP_FRAMEWORK_LIST_TOOLS_TIMEOUT_MS",
            DEFAULT_LIST_TOOLS_TIMEOUT_MS,
        ),
        tool_call: timeout_from_args_env(
            &args,
            "--call-timeout-ms",
            "MCP_FRAMEWORK_CALL_TIMEOUT_MS",
            DEFAULT_TOOL_CALL_TIMEOUT_MS,
        ),
    };

    let state = AppState {
        servers: Arc::new(Mutex::new(HashMap::new())),
        tool_routes: Arc::new(Mutex::new(HashMap::new())),
        timeouts: timeouts.clone(),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/mcp/framework-tools", get(framework_tools_handler))
        .route("/mcp/load", post(load_config_handler))
        .route("/mcp/tools", get(list_tools_handler))
        .route("/mcp/call", post(call_tool_handler))
        .layer(CorsLayer::permissive())
        .with_state(state);

    log_text(
        "启动",
        "main",
        &format!(
            "agentkernel-mcp-framework 已启动，监听地址=http://{addr}, log_dir={}, level={}, init_timeout_ms={}, list_tools_timeout_ms={}, call_timeout_ms={}",
            logging.log_dir.display(),
            logging.level,
            timeouts.initialize.as_millis(),
            timeouts.list_tools.as_millis(),
            timeouts.tool_call.as_millis()
        ),
    );
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn init_logging(args: &[String]) -> Result<LoggingRuntime, Box<dyn std::error::Error>> {
    let log_dir = resolve_log_dir(args)?;
    fs::create_dir_all(&log_dir)?;

    let level = arg_value(args, "--log-level")
        .or_else(|| std::env::var("MCP_FRAMEWORK_LOG_LEVEL").ok())
        .unwrap_or_else(|| DEFAULT_LOG_LEVEL.to_string());
    let env_filter = EnvFilter::try_new(level.clone())
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_LEVEL));

    let file_appender = tracing_appender::rolling::daily(&log_dir, "agentkernel-mcp-framework.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stdout)
                .with_target(false)
                .with_ansi(true),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(file_writer)
                .with_target(false)
                .with_ansi(false),
        )
        .init();

    Ok(LoggingRuntime {
        log_dir,
        level,
        _guard: guard,
    })
}

fn resolve_log_dir(args: &[String]) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(dir) = arg_value(args, "--log-dir")
        .or_else(|| std::env::var("MCP_FRAMEWORK_LOG_DIR").ok())
    {
        let path = PathBuf::from(dir);
        return Ok(if path.is_absolute() {
            path
        } else {
            std::env::current_dir()?.join(path)
        });
    }

    Ok(std::env::current_dir()?.join(DEFAULT_LOG_DIR_NAME))
}

fn timeout_from_args_env(args: &[String], flag: &str, env_key: &str, default_ms: u64) -> Duration {
    let value = arg_value(args, flag)
        .or_else(|| std::env::var(env_key).ok())
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default_ms);
    Duration::from_millis(value)
}

fn truncate_text(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let truncated: String = text.chars().take(limit).collect();
    format!("{truncated}...(truncated)")
}

fn log_text(kind: &str, scope: &str, text: &str) {
    info!(kind = %kind, scope = %scope, message = %truncate_text(text, LOG_PREVIEW_LIMIT));
}

fn log_json<T: Serialize>(kind: &str, scope: &str, value: &T) {
    match serde_json::to_string(value) {
        Ok(text) => log_text(kind, scope, &text),
        Err(err) => log_error_text("日志错误", scope, &format!("日志序列化失败: {err}")),
    }
}

fn log_debug_json<T: Serialize>(kind: &str, scope: &str, value: &T) {
    match serde_json::to_string(value) {
        Ok(text) => debug!(kind = %kind, scope = %scope, message = %truncate_text(&text, LOG_PREVIEW_LIMIT)),
        Err(err) => log_error_text("日志错误", scope, &format!("日志序列化失败: {err}")),
    }
}

fn log_warn_text(kind: &str, scope: &str, text: &str) {
    warn!(kind = %kind, scope = %scope, message = %truncate_text(text, LOG_PREVIEW_LIMIT));
}

fn log_error_text(kind: &str, scope: &str, text: &str) {
    error!(kind = %kind, scope = %scope, message = %truncate_text(text, LOG_PREVIEW_LIMIT));
}

fn masked_env(env: &HashMap<String, String>) -> Value {
    let mut masked = serde_json::Map::new();
    let mut keys: Vec<_> = env.keys().cloned().collect();
    keys.sort();
    for key in keys {
        masked.insert(key, Value::String("***masked***".to_string()));
    }
    Value::Object(masked)
}

fn sanitized_server_config(config: &McpServerConfig) -> Value {
    let mut env_keys: Vec<_> = config.env.keys().cloned().collect();
    env_keys.sort();
    json!({
        "command": config.command,
        "args": config.args,
        "env": masked_env(&config.env),
        "envKeys": env_keys,
        "envCount": config.env.len()
    })
}

fn sanitized_config_envelope(config: &ConfigEnvelope) -> Value {
    let mut servers = serde_json::Map::new();
    let mut items: Vec<_> = config.mcp_servers.iter().collect();
    items.sort_by(|(left, _), (right, _)| left.cmp(right));
    for (name, server) in items {
        servers.insert(name.clone(), sanitized_server_config(server));
    }
    json!({ "mcpServers": Value::Object(servers) })
}

fn sanitized_load_request(req: &LoadConfigRequest) -> Value {
    json!({
        "configPath": req.config_path,
        "config": req.config.as_ref().map(sanitized_config_envelope)
    })
}

fn summarize_failed_servers(failed_servers: &[FailedServerInfo]) -> String {
    failed_servers
        .iter()
        .map(|item| format!("{}:{}:{}", item.name, item.stage, item.error))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn arg_value(args: &[String], key: &str) -> Option<String> {
    args.iter().position(|a| a == key).and_then(|i| args.get(i + 1)).cloned()
}

async fn health() -> Json<Value> {
    let started_at = Instant::now();
    log_text("HTTP请求", "GET /health", "{}");
    let data = json!({
        "ok": true,
        "service": "agentkernel-mcp-framework",
        "version": env!("CARGO_PKG_VERSION")
    });
    log_json("HTTP响应", "GET /health", &data);
    log_text("HTTP完成", "GET /health", &format!("duration_ms={}", started_at.elapsed().as_millis()));
    Json(data)
}

async fn framework_tools_handler() -> Json<ApiResponse<Vec<ToolDefinition>>> {
    let started_at = Instant::now();
    log_text("HTTP请求", "GET /mcp/framework-tools", "{}");
    let data = framework_tools();
    log_json("HTTP响应", "GET /mcp/framework-tools", &data);
    log_text(
        "HTTP完成",
        "GET /mcp/framework-tools",
        &format!("duration_ms={} tool_count={}", started_at.elapsed().as_millis(), data.len()),
    );
    Json(ApiResponse { ok: true, data })
}

async fn load_config_handler(
    State(state): State<AppState>,
    Json(req): Json<LoadConfigRequest>,
) -> Result<Json<ApiResponse<Value>>, FrameworkError> {
    let started_at = Instant::now();
    log_json("HTTP请求", "POST /mcp/load", &sanitized_load_request(&req));
    match load_config(&state, req).await {
        Ok(data) => {
            log_json("HTTP响应", "POST /mcp/load", &data);
            log_text(
                "HTTP完成",
                "POST /mcp/load",
                &format!("duration_ms={}", started_at.elapsed().as_millis()),
            );
            Ok(Json(ApiResponse { ok: true, data }))
        }
        Err(err) => {
            log_error_text(
                "HTTP失败",
                "POST /mcp/load",
                &format!("duration_ms={} err={err}", started_at.elapsed().as_millis()),
            );
            Err(err)
        }
    }
}

async fn list_tools_handler(State(state): State<AppState>) -> Result<Json<ApiResponse<Value>>, FrameworkError> {
    let started_at = Instant::now();
    log_text("HTTP请求", "GET /mcp/tools", "{}");
    match list_loaded_tools(&state).await {
        Ok(data) => {
            log_json("HTTP响应", "GET /mcp/tools", &data);
            log_text(
                "HTTP完成",
                "GET /mcp/tools",
                &format!("duration_ms={}", started_at.elapsed().as_millis()),
            );
            Ok(Json(ApiResponse { ok: true, data }))
        }
        Err(err) => {
            log_error_text(
                "HTTP失败",
                "GET /mcp/tools",
                &format!("duration_ms={} err={err}", started_at.elapsed().as_millis()),
            );
            Err(err)
        }
    }
}

async fn call_tool_handler(
    State(state): State<AppState>,
    Json(req): Json<CallToolRequest>,
) -> Result<Json<ApiResponse<Value>>, FrameworkError> {
    let started_at = Instant::now();
    log_json("HTTP请求", "POST /mcp/call", &req);
    match call_tool_proxy(&state, req).await {
        Ok(data) => {
            log_json("HTTP响应", "POST /mcp/call", &data);
            log_text(
                "HTTP完成",
                "POST /mcp/call",
                &format!("duration_ms={}", started_at.elapsed().as_millis()),
            );
            Ok(Json(ApiResponse { ok: true, data }))
        }
        Err(err) => {
            log_error_text(
                "HTTP失败",
                "POST /mcp/call",
                &format!("duration_ms={} err={err}", started_at.elapsed().as_millis()),
            );
            Err(err)
        }
    }
}

fn framework_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "mcp.load_config".into(),
            description: "加载 MCP 配置单，启动其中的 MCP Server，并发现全部工具".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "configPath": { "type": "string", "description": "MCP 配置文件路径" },
                    "config": { "type": "object", "description": "内联 MCP 配置，格式为 { mcpServers: {...} }" }
                }
            }),
        },
        ToolDefinition {
            name: "mcp.list_tools".into(),
            description: "列出已经加载的 MCP 工具，返回可注册给 AgentKernel 的工具清单".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDefinition {
            name: "mcp.call_tool".into(),
            description: "代理执行已经发现的 MCP 工具".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "业务端注册时拿到的工具名，如 mcp.local-code-suite.read" },
                    "arguments": { "type": "object" }
                },
                "required": ["name"]
            }),
        },
    ]
}

async fn load_config(state: &AppState, req: LoadConfigRequest) -> Result<Value, FrameworkError> {
    let total_started_at = Instant::now();
    let config = if let Some(config) = req.config {
        config
    } else if let Some(path) = req.config_path {
        let text = fs::read_to_string(path)?;
        serde_json::from_str(&text)?
    } else {
        return Err(FrameworkError::InvalidRequest("config or configPath is required".into()));
    };

    let previous_server_count = {
        let servers = state.servers.lock().await;
        servers.len()
    };
    let mut loaded = Vec::new();
    let mut tools_for_register = Vec::new();
    let mut merged_servers = {
        let servers = state.servers.lock().await;
        servers.clone()
    };
    let mut merged_routes = {
        let routes = state.tool_routes.lock().await;
        routes.clone()
    };
    let mut failed_servers = Vec::new();
    let requested_server_count = config.mcp_servers.len();
    log_text(
        "配置加载开始",
        "load_config",
        &format!(
            "server_count={} init_timeout_ms={} list_tools_timeout_ms={}",
            requested_server_count,
            state.timeouts.initialize.as_millis(),
            state.timeouts.list_tools.as_millis()
        ),
    );

    for (server_name, server_config) in config.mcp_servers {
        let server_started_at = Instant::now();
        log_json("框架启动MCP服务", &server_name, &sanitized_server_config(&server_config));

        let start_stage_at = Instant::now();
        let mut managed = match ManagedServer::start(server_name.clone(), server_config.clone()).await {
            Ok(managed) => managed,
            Err(err) => {
                failed_servers.push(FailedServerInfo {
                    name: server_name.clone(),
                    command: server_config.command.clone(),
                    args: server_config.args.clone(),
                    stage: "start".to_string(),
                    error: err.to_string(),
                    duration_ms: start_stage_at.elapsed().as_millis(),
                });
                log_error_text(
                    "MCP服务启动失败",
                    &server_name,
                    &format!("duration_ms={} err={err}", start_stage_at.elapsed().as_millis()),
                );
                continue;
            }
        };
        log_text(
            "MCP服务启动完成",
            &server_name,
            &format!("duration_ms={}", start_stage_at.elapsed().as_millis()),
        );

        let init_stage_at = Instant::now();
        if let Err(err) = managed.initialize(state.timeouts.initialize).await {
            failed_servers.push(FailedServerInfo {
                name: server_name.clone(),
                command: server_config.command.clone(),
                args: server_config.args.clone(),
                stage: "initialize".to_string(),
                error: err.to_string(),
                duration_ms: init_stage_at.elapsed().as_millis(),
            });
            log_error_text(
                "MCP初始化失败",
                &server_name,
                &format!("duration_ms={} err={err}", init_stage_at.elapsed().as_millis()),
            );
            continue;
        }
        log_text(
            "MCP初始化完成",
            &server_name,
            &format!("duration_ms={}", init_stage_at.elapsed().as_millis()),
        );

        let list_stage_at = Instant::now();
        let tools = match managed.list_tools(state.timeouts.list_tools).await {
            Ok(tools) => tools,
            Err(err) => {
                failed_servers.push(FailedServerInfo {
                    name: server_name.clone(),
                    command: server_config.command.clone(),
                    args: server_config.args.clone(),
                    stage: "list_tools".to_string(),
                    error: err.to_string(),
                    duration_ms: list_stage_at.elapsed().as_millis(),
                });
                log_error_text(
                    "MCP列工具失败",
                    &server_name,
                    &format!("duration_ms={} err={err}", list_stage_at.elapsed().as_millis()),
                );
                continue;
            }
        };
        managed.tools = tools.clone();
        log_text(
            "MCP列工具完成",
            &server_name,
            &format!(
                "duration_ms={} tool_count={}",
                list_stage_at.elapsed().as_millis(),
                managed.tools.len()
            ),
        );

        merged_routes.retain(|_, route| route.server_name != server_name);

        for tool in tools {
            let exposed_name = format!("mcp.{}.{}", server_name, tool.name);
            merged_routes.insert(exposed_name.clone(), ToolRoute {
                server_name: server_name.clone(),
                remote_tool_name: tool.name.clone(),
            });
            tools_for_register.push(json!({
                "name": exposed_name,
                "description": format!("[{}] {}", server_name, tool.description),
                "inputSchema": tool.input_schema,
                "mcp": {
                    "server": server_name,
                    "remoteTool": tool.name
                }
            }));
        }

        loaded.push(json!({
            "name": server_name,
            "command": server_config.command,
            "args": server_config.args,
            "toolCount": managed.tools.len()
        }));
        merged_servers.insert(server_name, Arc::new(Mutex::new(managed)));
        log_text(
            "MCP服务加载完成",
            loaded.last().and_then(|value| value.get("name")).and_then(|value| value.as_str()).unwrap_or("unknown"),
            &format!("duration_ms={}", server_started_at.elapsed().as_millis()),
        );
    }

    if requested_server_count > 0 && loaded.is_empty() && !failed_servers.is_empty() {
        log_warn_text(
            "配置加载全部失败",
            "load_config",
            &format!(
                "duration_ms={} requested_server_count={} existing_server_count={} failed={}",
                total_started_at.elapsed().as_millis(),
                requested_server_count,
                previous_server_count,
                summarize_failed_servers(&failed_servers)
            ),
        );
        return Err(FrameworkError::Mcp(format!(
            "all MCP servers failed to load, keeping previous state: {}",
            summarize_failed_servers(&failed_servers)
        )));
    }

    {
        let mut servers = state.servers.lock().await;
        *servers = merged_servers;
    }
    {
        let mut routes = state.tool_routes.lock().await;
        *routes = merged_routes;
    }

    log_text(
        "配置加载完成",
        "load_config",
        &format!(
            "duration_ms={} requested_server_count={} loaded_servers={} failed_servers={} previous_server_count={} current_server_count={} tool_count={}",
            total_started_at.elapsed().as_millis(),
            requested_server_count,
            loaded.len(),
            failed_servers.len(),
            previous_server_count,
            {
                let servers = state.servers.lock().await;
                servers.len()
            },
            tools_for_register.len()
        ),
    );
    if !failed_servers.is_empty() {
        log_warn_text(
            "配置加载部分失败",
            "load_config",
            &summarize_failed_servers(&failed_servers),
        );
    }

    Ok(json!({
        "loadedServers": loaded,
        "failedServers": failed_servers,
        "loadedServerCount": loaded.len(),
        "failedServerCount": failed_servers.len(),
        "tools": tools_for_register,
        "toolCount": tools_for_register.len()
    }))
}

async fn list_loaded_tools(state: &AppState) -> Result<Value, FrameworkError> {
    let started_at = Instant::now();
    let servers = state.servers.lock().await;
    let mut tools = Vec::new();
    for (server_name, server) in servers.iter() {
        let server = server.lock().await;
        for tool in &server.tools {
            tools.push(json!({
                "name": format!("mcp.{}.{}", server_name, tool.name),
                "description": format!("[{}] {}", server_name, tool.description),
                "inputSchema": tool.input_schema,
                "mcp": {
                    "server": server_name,
                    "remoteTool": tool.name
                }
            }));
        }
    }
    log_text(
        "列出已加载工具",
        "list_loaded_tools",
        &format!(
            "duration_ms={} server_count={} tool_count={}",
            started_at.elapsed().as_millis(),
            servers.len(),
            tools.len()
        ),
    );
    Ok(json!({ "tools": tools, "toolCount": tools.len() }))
}

async fn call_tool_proxy(state: &AppState, req: CallToolRequest) -> Result<Value, FrameworkError> {
    let started_at = Instant::now();
    let route = {
        let routes = state.tool_routes.lock().await;
        routes.get(&req.name).cloned().ok_or_else(|| FrameworkError::ToolNotFound(req.name.clone()))?
    };
    let server = {
        let servers = state.servers.lock().await;
        servers.get(&route.server_name).cloned().ok_or_else(|| FrameworkError::ServerNotFound(route.server_name.clone()))?
    };
    log_text(
        "工具调用开始",
        &req.name,
        &format!(
            "server={} remote_tool={} timeout_ms={}",
            route.server_name,
            route.remote_tool_name,
            state.timeouts.tool_call.as_millis()
        ),
    );
    let mut server = server.lock().await;
    let result = server.call_tool(&route.remote_tool_name, req.arguments, state.timeouts.tool_call).await;
    match &result {
        Ok(_) => log_text(
            "工具调用完成",
            &route.remote_tool_name,
            &format!("duration_ms={} server={}", started_at.elapsed().as_millis(), route.server_name),
        ),
        Err(err) => log_error_text(
            "工具调用失败",
            &route.remote_tool_name,
            &format!("duration_ms={} server={} err={err}", started_at.elapsed().as_millis(), route.server_name),
        ),
    }
    result
}

impl ManagedServer {
    async fn start(name: String, config: McpServerConfig) -> Result<Self, FrameworkError> {
        let current_dir = std::env::current_dir().ok();
        log_json("MCP进程启动参数", &name, &sanitized_server_config(&config));
        log_text(
            "MCP进程启动上下文",
            &name,
            &format!(
                "cwd={} command_exists={}",
                current_dir
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "<unknown>".to_string()),
                Path::new(&config.command).exists()
            ),
        );
        let mut command = Command::new(&config.command);
        command.args(&config.args);
        command.envs(&config.env);
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                log_error_text("MCP进程启动失败", &name, &err.to_string());
                return Err(err.into());
            }
        };
        log_text(
            "MCP进程已启动",
            &name,
            &format!("pid={}", child.id().map(|id| id.to_string()).unwrap_or_else(|| "unknown".to_string())),
        );
        let stdin = child.stdin.take().ok_or_else(|| FrameworkError::Mcp("failed to open child stdin".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| FrameworkError::Mcp("failed to open child stdout".into()))?;
        let stderr = child.stderr.take().ok_or_else(|| FrameworkError::Mcp("failed to open child stderr".into()))?;
        spawn_stderr_logger(name.clone(), stderr);

        Ok(Self {
            name,
            child,
            stdin,
            stdout: BufReader::new(stdout),
            request_id: 0,
            tools: Vec::new(),
        })
    }

    async fn initialize(&mut self, timeout_duration: Duration) -> Result<(), FrameworkError> {
        let started_at = Instant::now();
        let _ = self.request("initialize", json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {
                "name": "agentkernel-mcp-framework",
                "version": env!("CARGO_PKG_VERSION")
            }
        }), timeout_duration).await?;
        self.notify("notifications/initialized", json!({})).await?;
        log_text(
            "MCP初始化通知完成",
            &self.name,
            &format!(
                "duration_ms={} timeout_ms={}",
                started_at.elapsed().as_millis(),
                timeout_duration.as_millis()
            ),
        );
        Ok(())
    }

    async fn list_tools(&mut self, timeout_duration: Duration) -> Result<Vec<ToolDefinition>, FrameworkError> {
        let started_at = Instant::now();
        let result = self.request("tools/list", json!({}), timeout_duration).await?;
        let tools = result.get("tools").cloned().unwrap_or_else(|| json!([]));
        let parsed = serde_json::from_value(tools).map_err(FrameworkError::from)?;
        log_text(
            "MCP工具解析完成",
            &self.name,
            &format!(
                "duration_ms={} timeout_ms={}",
                started_at.elapsed().as_millis(),
                timeout_duration.as_millis()
            ),
        );
        Ok(parsed)
    }

    async fn call_tool(&mut self, name: &str, arguments: Value, timeout_duration: Duration) -> Result<Value, FrameworkError> {
        log_debug_json("MCP工具参数", name, &arguments);
        self.request("tools/call", json!({
            "name": name,
            "arguments": arguments
        }), timeout_duration).await
    }

    async fn request(&mut self, method: &str, params: Value, timeout_duration: Duration) -> Result<Value, FrameworkError> {
        let started_at = Instant::now();
        self.request_id += 1;
        let id = self.request_id;
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        log_json("MCP请求", &self.name, &request);
        self.write_message(&request).await?;
        log_text(
            "MCP等待响应",
            &self.name,
            &format!(
                "request_id={} method={} timeout_ms={}",
                id,
                method,
                timeout_duration.as_millis()
            ),
        );
        let response = match timeout(timeout_duration, self.read_message()).await {
            Ok(result) => result?,
            Err(_) => {
                log_error_text(
                    "MCP响应超时",
                    &self.name,
                    &format!(
                        "request_id={} method={} timeout_ms={} duration_ms={}",
                        id,
                        method,
                        timeout_duration.as_millis(),
                        started_at.elapsed().as_millis()
                    ),
                );
                return Err(FrameworkError::Mcp(format!(
                    "timeout waiting for MCP response: method={} timeout_ms={}",
                    method,
                    timeout_duration.as_millis()
                )));
            }
        };
        log_json("MCP响应", &self.name, &response);
        if let Some(error) = response.get("error") {
            log_error_text(
                "MCP响应错误",
                &self.name,
                &format!("request_id={} method={} duration_ms={} error={}", id, method, started_at.elapsed().as_millis(), error),
            );
            return Err(FrameworkError::Mcp(error.to_string()));
        }
        log_text(
            "MCP请求完成",
            &self.name,
            &format!("request_id={} method={} duration_ms={}", id, method, started_at.elapsed().as_millis()),
        );
        Ok(response.get("result").cloned().unwrap_or_else(|| json!({})))
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), FrameworkError> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });
        log_json("MCP通知", &self.name, &notification);
        self.write_message(&notification).await
    }

    async fn write_message(&mut self, message: &Value) -> Result<(), FrameworkError> {
        let text = serde_json::to_string(message)?;
        debug!(kind = "MCP写入", scope = %self.name, bytes = text.len(), message = %truncate_text(&text, LOG_PREVIEW_LIMIT));
        self.stdin.write_all(text.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn read_message(&mut self) -> Result<Value, FrameworkError> {
        let started_at = Instant::now();
        let mut line = String::new();
        let bytes = self.stdout.read_line(&mut line).await?;
        if bytes == 0 {
            log_error_text("MCP标准输出关闭", &self.name, "mcp server closed stdout");
            return Err(FrameworkError::Mcp("mcp server closed stdout".into()));
        }
        let trimmed = line.trim();
        log_text(
            "MCP标准输出",
            &self.name,
            &format!("duration_ms={} bytes={} payload={}", started_at.elapsed().as_millis(), bytes, truncate_text(trimmed, LOG_PREVIEW_LIMIT)),
        );
        serde_json::from_str(trimmed).map_err(FrameworkError::from)
    }
}

fn spawn_stderr_logger(server_name: String, stderr: ChildStderr) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => log_warn_text("MCP标准错误", &server_name, line.trim()),
                Err(err) => {
                    log_error_text("MCP标准错误异常", &server_name, &err.to_string());
                    break;
                }
            }
        }
    });
}

impl Drop for ManagedServer {
    fn drop(&mut self) {
        log_warn_text("MCP进程回收", &self.name, "drop -> start_kill");
        let _ = self.child.start_kill();
    }
}
