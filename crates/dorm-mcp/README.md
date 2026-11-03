# dorm-mcp

A native [Model Context Protocol](https://modelcontextprotocol.io) server over
your [DORM](https://github.com/ngolacloud/dorm) models. It exposes the model
registry as MCP tools, resources and prompts — all generated from metadata,
nothing written per model — so an agent can find and read your data safely:
through the dynamic query path, never raw SQL, and never across a tenant
boundary. See [ADR 0020](https://github.com/ngolacloud/dorm/blob/main/docs/adr/0020-mcp-server.md).

```sh
dorm mcp generate            # writes src/bin/mcp.rs over your models
cargo run --bin mcp -- --transport stdio --tenant acme
```

Configure it in Claude Desktop / Claude Code:

```json
{ "command": "cargo", "args": ["run","--bin","mcp","--","--transport","stdio"] }
```

Read-only in this release: `list_models`, `describe_model`, `query`, `get`,
`count`, `aggregate`, `semantic_search` (over `dorm-rag`). The write tools
(`insert`/`update`/`delete` behind `--write`, with `dry_run` and row caps) and
HTTP/OAuth transport are the next brick.

The transport is JSON-RPC 2.0 over stdio, implemented directly on `serde_json`
(no `rmcp`), so the crate builds offline at MSRV 1.75.
