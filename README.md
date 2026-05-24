# AgentKernel MCP Framework

English version: [README.en.md](README.en.md)

友情链接：

- AgentKernel: https://github.com/cih1996/AgentKernel
- AgentKernel Capabilities: https://github.com/cih1996/agentkernel-capabilities

---

AgentKernel MCP Framework 是一个独立的通用 MCP 发现与执行服务。它对业务端提供 HTTP API，对后端 MCP Server 使用 stdio 连接。

它的职责是：

1. 接收业务端传来的 MCP 配置单。
2. 启动配置里的 MCP Server。
3. 执行 `initialize`、`notifications/initialized`、`tools/list`。
4. 把所有 MCP 工具整理成业务端可注册给 AgentKernel 的工具清单。
5. 接收业务端转发的工具调用，并代理执行远端 MCP `tools/call`。

它和 [AgentKernel Capabilities](https://github.com/cih1996/agentkernel-capabilities) 是两个独立项目：

- `agentkernel-capabilities`：具体能力服务，提供 `glob/read/bash` 等工具。
- `agentkernel-mcp-framework`：通用 MCP 发现和执行框架，负责加载、发现、路由、代理调用。

## 推荐链路

```text
AgentKernel
  -> Business App
  -> HTTP: agentkernel-mcp-framework
  -> stdio: MCP Servers
  -> tools/call
```

完整流程：

```text
启动 AgentKernel
  -> 业务端调用 agentkernel-mcp-framework 的 HTTP API
  -> 业务端发“配置单”给 Framework
  -> Framework 启动配置内的 MCP Server
  -> Framework 返回所有工具清单
  -> 业务端把工具注册给 AgentKernel
  -> 业务端发起对话
  -> AgentKernel 请求调用工具
  -> 业务端判断工具是否以 mcp. 开头
  -> 是：调用 Framework 的 HTTP /mcp/call
  -> Framework 代理执行真实 MCP 工具
  -> 业务端把 ToolResult 回填给 AgentKernel
```

## 启动

```bash
cargo run --release -- --host 127.0.0.1 --port 19528
```

或使用 release 产物：

```bash
target/release/agentkernel-mcp-framework --host 127.0.0.1 --port 19528
```

## HTTP API

### 健康检查

```http
GET /health
```

### 查看框架自身工具

```http
GET /mcp/framework-tools
```

返回框架暴露给业务端的 3 个工具定义：

- `mcp.load_config`
- `mcp.list_tools`
- `mcp.call_tool`

### 加载 MCP 配置单

```http
POST /mcp/load
Content-Type: application/json
```

请求体可以传 `configPath`：

```json
{
  "configPath": "/absolute/path/to/example.mcp.json"
}
```

也可以直接传 `config`：

```json
{
  "config": {
    "mcpServers": {
      "local-code-suite": {
        "command": "/absolute/path/to/agentkernel-capabilities/target/release/agentkernel-capabilities",
        "args": ["--workspace", "/absolute/path/to/workspace"],
        "env": {}
      }
    }
  }
}
```

返回结构重点：

```json
{
  "ok": true,
  "data": {
    "loadedServers": [
      {
        "name": "local-code-suite",
        "command": "...",
        "args": [],
        "toolCount": 6
      }
    ],
    "tools": [
      {
        "name": "mcp.local-code-suite.read",
        "description": "[local-code-suite] Read file contents with pagination",
        "inputSchema": {},
        "mcp": {
          "server": "local-code-suite",
          "remoteTool": "read"
        }
      }
    ],
    "toolCount": 6
  }
}
```

业务端把 `data.tools` 注册给 AgentKernel 即可。

### 获取已加载工具

```http
GET /mcp/tools
```

### 代理调用工具

```http
POST /mcp/call
Content-Type: application/json
```

请求体：

```json
{
  "name": "mcp.local-code-suite.read",
  "arguments": {
    "file_path": "/absolute/path/to/Cargo.toml",
    "offset": 0,
    "limit": 80
  }
}
```

返回值会透传远端 MCP Server 的 `tools/call` result。

## 工具命名规则

Framework 对外暴露给业务端注册的 MCP 工具名固定为：

```text
mcp.<serverName>.<remoteToolName>
```

例如：

```text
mcp.local-code-suite.read
mcp.local-code-suite.bash
```

业务端只要检测工具名是否以 `mcp.` 开头，就能决定是否转发给 Framework。

## 示例配置

项目内提供：

- `example.mcp.json`

## 当前限制

- 当前只支持 stdio MCP Server 作为后端。
- `POST /mcp/load` 会替换当前已加载 MCP Server，而不是增量合并。
- 子 MCP Server 的 stderr 默认丢弃，避免污染框架输出。
