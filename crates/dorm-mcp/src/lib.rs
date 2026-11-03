//! `dorm-mcp` — a native Model Context Protocol server over your DORM models
//! (ADR 0020).
//!
//! It exposes the registry as MCP **tools** (`list_models`, `describe_model`,
//! `query`, `get`, `count`, `aggregate`, `semantic_search`), **resources**
//! (`dorm://models`, `dorm://models/{name}`, `dorm://records/{model}/{pk}`) and
//! **prompts** — all generated from the same `ModelMeta`/`FieldMeta` the compiler
//! reads, nothing written per model. An agent reaches the data only through the
//! dynamic query path, so it can name no table or column the derive did not emit,
//! splice no SQL, and — for a `tenant_scoped` model — never cross the tenant,
//! which is forced from the server config, never a tool argument.
//!
//! This is **brick 1** of ADR 0020: the read-only server and `dorm mcp generate`.
//! Writes (`insert`/`update`/`delete` with `--write`, `dry_run`, caps), HTTP auth
//! and the full audit event land in brick 2.
//!
//! The transport is JSON-RPC 2.0 over stdio, implemented directly (not `rmcp`),
//! so the crate builds offline at MSRV 1.75 — see ADR 0020 for why.

#![forbid(unsafe_code)]

mod convert;
mod http;
mod oauth;
mod protocol;
mod resources;
mod schema;
mod server;
mod tools;

pub use http::{serve_http, serve_on};
pub use oauth::{parse_clients, OAuthClient};
pub use server::{handle_message, parse_tokens, serve, HttpToken, ServerConfig};
