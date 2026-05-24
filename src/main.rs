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
use std::process::Stdio;
use std::sync::Arc;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tower_http::cors::CorsLayer;

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

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigEnvelope {
    #[serde(default)]
    mcp_servers: HashMap<String, McpServerConfig>,
}

#[derive(Debug, Clone, Deserialize)]
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
}

#[derive(Debug, Clone)]
struct ToolRoute {
    server_name: String,
    remote_tool_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoadConfigRequest {
    #[serde(default)]
    config: Option<ConfigEnvelope>,
    #[serde(default)]
    config_path: Option<String>,
}

#[derive(Debug, Deserialize)]
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let host = arg_value(&args, "--host").unwrap_or_else(|| "127.0.0.1".to_string());
    let port = arg_value(&args, "--port").unwrap_or_else(|| "9528".to_string());
    let addr: SocketAddr = format!("{host}:{port}").parse()?;

    let state = AppState {
        servers: Arc::new(Mutex::new(HashMap::new())),
        tool_routes: Arc::new(Mutex::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/mcp/framework-tools", get(framework_tools_handler))
        .route("/mcp/load", post(load_config_handler))
        .route("/mcp/tools", get(list_tools_handler))
        .route("/mcp/call", post(call_tool_handler))
        .layer(CorsLayer::permissive())
        .with_state(state);

    println!("agentkernel-mcp-framework listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn arg_value(args: &[String], key: &str) -> Option<String> {
    args.iter().position(|a| a == key).and_then(|i| args.get(i + 1)).cloned()
}

async fn health() -> Json<Value> {
    Json(json!({
        "ok": true,
        "service": "agentkernel-mcp-framework",
        "version": env!("CARGO_PKG_VERSION")
    }))
}

async fn framework_tools_handler() -> Json<ApiResponse<Vec<ToolDefinition>>> {
    Json(ApiResponse { ok: true, data: framework_tools() })
}

async fn load_config_handler(
    State(state): State<AppState>,
    Json(req): Json<LoadConfigRequest>,
) -> Result<Json<ApiResponse<Value>>, FrameworkError> {
    let data = load_config(&state, req).await?;
    Ok(Json(ApiResponse { ok: true, data }))
}

async fn list_tools_handler(State(state): State<AppState>) -> Result<Json<ApiResponse<Value>>, FrameworkError> {
    let data = list_loaded_tools(&state).await?;
    Ok(Json(ApiResponse { ok: true, data }))
}

async fn call_tool_handler(
    State(state): State<AppState>,
    Json(req): Json<CallToolRequest>,
) -> Result<Json<ApiResponse<Value>>, FrameworkError> {
    let data = call_tool_proxy(&state, req).await?;
    Ok(Json(ApiResponse { ok: true, data }))
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
    let config = if let Some(config) = req.config {
        config
    } else if let Some(path) = req.config_path {
        let text = fs::read_to_string(path)?;
        serde_json::from_str(&text)?
    } else {
        return Err(FrameworkError::InvalidRequest("config or configPath is required".into()));
    };

    let mut loaded = Vec::new();
    let mut tools_for_register = Vec::new();
    let mut new_servers = HashMap::new();
    let mut new_routes = HashMap::new();

    for (server_name, server_config) in config.mcp_servers {
        let mut managed = ManagedServer::start(server_config.clone()).await?;
        managed.initialize().await?;
        let tools = managed.list_tools().await?;
        managed.tools = tools.clone();

        for tool in tools {
            let exposed_name = format!("mcp.{}.{}", server_name, tool.name);
            new_routes.insert(exposed_name.clone(), ToolRoute {
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
        new_servers.insert(server_name, Arc::new(Mutex::new(managed)));
    }

    {
        let mut servers = state.servers.lock().await;
        *servers = new_servers;
    }
    {
        let mut routes = state.tool_routes.lock().await;
        *routes = new_routes;
    }

    Ok(json!({
        "loadedServers": loaded,
        "tools": tools_for_register,
        "toolCount": tools_for_register.len()
    }))
}

async fn list_loaded_tools(state: &AppState) -> Result<Value, FrameworkError> {
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
    Ok(json!({ "tools": tools, "toolCount": tools.len() }))
}

async fn call_tool_proxy(state: &AppState, req: CallToolRequest) -> Result<Value, FrameworkError> {
    let route = {
        let routes = state.tool_routes.lock().await;
        routes.get(&req.name).cloned().ok_or_else(|| FrameworkError::ToolNotFound(req.name.clone()))?
    };
    let server = {
        let servers = state.servers.lock().await;
        servers.get(&route.server_name).cloned().ok_or_else(|| FrameworkError::ServerNotFound(route.server_name.clone()))?
    };
    let mut server = server.lock().await;
    server.call_tool(&route.remote_tool_name, req.arguments).await
}

impl ManagedServer {
    async fn start(config: McpServerConfig) -> Result<Self, FrameworkError> {
        let mut command = Command::new(&config.command);
        command.args(&config.args);
        command.envs(&config.env);
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());

        let mut child = command.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| FrameworkError::Mcp("failed to open child stdin".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| FrameworkError::Mcp("failed to open child stdout".into()))?;

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            request_id: 0,
            tools: Vec::new(),
        })
    }

    async fn initialize(&mut self) -> Result<(), FrameworkError> {
        let _ = self.request("initialize", json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {
                "name": "agentkernel-mcp-framework",
                "version": env!("CARGO_PKG_VERSION")
            }
        })).await?;
        self.notify("notifications/initialized", json!({})).await?;
        Ok(())
    }

    async fn list_tools(&mut self) -> Result<Vec<ToolDefinition>, FrameworkError> {
        let result = self.request("tools/list", json!({})).await?;
        let tools = result.get("tools").cloned().unwrap_or_else(|| json!([]));
        serde_json::from_value(tools).map_err(FrameworkError::from)
    }

    async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value, FrameworkError> {
        self.request("tools/call", json!({
            "name": name,
            "arguments": arguments
        })).await
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, FrameworkError> {
        self.request_id += 1;
        let id = self.request_id;
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        self.write_message(&request).await?;
        let response = self.read_message().await?;
        if let Some(error) = response.get("error") {
            return Err(FrameworkError::Mcp(error.to_string()));
        }
        Ok(response.get("result").cloned().unwrap_or_else(|| json!({})))
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), FrameworkError> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });
        self.write_message(&notification).await
    }

    async fn write_message(&mut self, message: &Value) -> Result<(), FrameworkError> {
        let text = serde_json::to_string(message)?;
        self.stdin.write_all(text.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn read_message(&mut self) -> Result<Value, FrameworkError> {
        let mut line = String::new();
        let bytes = self.stdout.read_line(&mut line).await?;
        if bytes == 0 {
            return Err(FrameworkError::Mcp("mcp server closed stdout".into()));
        }
        serde_json::from_str(line.trim()).map_err(FrameworkError::from)
    }
}

impl Drop for ManagedServer {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}
