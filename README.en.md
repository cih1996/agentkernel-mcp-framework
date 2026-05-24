# AgentKernel MCP Framework

中文版本: [README.md](README.md)

Related projects:

- AgentKernel: https://github.com/cih1996/AgentKernel
- AgentKernel Capabilities: https://github.com/cih1996/agentkernel-capabilities

---

AgentKernel MCP Framework is an independent generic MCP discovery and execution service. It exposes HTTP APIs to your business layer and connects to backend MCP servers through stdio.

Its responsibilities are:

1. Receive MCP configuration from the business layer.
2. Start MCP servers defined in the config.
3. Run `initialize`, `notifications/initialized`, and `tools/list`.
4. Convert discovered MCP tools into tool definitions that can be registered into AgentKernel.
5. Receive tool-call requests from the business layer and proxy them to remote MCP `tools/call`.

It can work with [AgentKernel Capabilities](https://github.com/cih1996/agentkernel-capabilities):

- `agentkernel-capabilities`: concrete capability server exposing tools such as `glob/read/bash`.
- `agentkernel-mcp-framework`: generic MCP discovery/execution framework for loading, routing, and proxying tool calls.

## Recommended Flow

```text
AgentKernel
  -> Business App
  -> HTTP: agentkernel-mcp-framework
  -> stdio: MCP Servers
  -> tools/call
```

Full flow:

```text
Start AgentKernel
  -> Business app calls the Framework HTTP API
  -> Business app sends MCP config to the Framework
  -> Framework starts MCP servers from the config
  -> Framework returns discovered tool definitions
  -> Business app registers tools into AgentKernel
  -> Business app starts a conversation
  -> AgentKernel requests a tool call
  -> Business app checks whether the tool name starts with mcp.
  -> If yes, call Framework HTTP /mcp/call
  -> Framework proxies the call to the real MCP server
  -> Business app sends ToolResult back to AgentKernel
```

## Start

```bash
cargo run --release -- --host 127.0.0.1 --port 19528
```

Or use the release binary:

```bash
target/release/agentkernel-mcp-framework --host 127.0.0.1 --port 19528
```

## HTTP API

### Health Check

```http
GET /health
```

### Framework Tools

```http
GET /mcp/framework-tools
```

Returns three framework tool definitions:

- `mcp.load_config`
- `mcp.list_tools`
- `mcp.call_tool`

### Load MCP Config

```http
POST /mcp/load
Content-Type: application/json
```

Request body with `configPath`:

```json
{
  "configPath": "/absolute/path/to/example.mcp.json"
}
```

Or inline `config`:

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

Important response fields:

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

Register `data.tools` into AgentKernel from the business layer.

### List Loaded Tools

```http
GET /mcp/tools
```

### Proxy Tool Call

```http
POST /mcp/call
Content-Type: application/json
```

Request body:

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

The response proxies the remote MCP server's `tools/call` result.

## Tool Naming Rule

The framework exposes MCP tools using this naming convention:

```text
mcp.<serverName>.<remoteToolName>
```

For example:

```text
mcp.local-code-suite.read
mcp.local-code-suite.bash
```

Your business layer only needs to check whether the tool name starts with `mcp.` to decide whether to forward it to the Framework.

## Example Config

Included in this repo:

- `example.mcp.json`

## Current Limitations

- Only stdio MCP servers are supported as backends.
- `POST /mcp/load` replaces currently loaded MCP servers instead of merging incrementally.
- Child MCP server stderr is discarded by default to keep the framework output clean.
