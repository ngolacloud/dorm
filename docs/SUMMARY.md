# Summary

[Introduction](introduction.md)

# Getting started

- [Installation](installation.md)
- [Start a new project](quickstart.md)
- [Use DORM in an existing project](existing-project.md)

# Using DORM

- [The schema](schema.md)
- [Models](models.md)
- [Making queries](queries.md)
- [Migrations](migrations.md)
- [Connection & transactions](connection-and-transactions.md)
- [Managing your database](managing-the-database.md)
- [DORM Studio](studio.md)

# Reference

- [CLI reference](cli-contract.md)
- [API reference (docs.rs)](api-reference.md)
- [Roadmap](roadmap.md)
- [Architecture decisions](adr/0001-declarative-schema.md)
  - [0001 — Declarative schema](adr/0001-declarative-schema.md)
  - [0002 — Multi-backend](adr/0002-multi-backend.md)
  - [0003 — Backend-generic db](adr/0003-backend-generic-db.md)
  - [0004 — Production connections](adr/0004-production-connections.md)
  - [0005 — Raw SQL](adr/0005-raw-sql.md)
  - [0006 — dbshell DML](adr/0006-dbshell-dml.md)
  - [0007 — Prepared-statement cache & benchmarks](adr/0007-prepared-statement-cache-and-benchmarks.md)
  - [0008 — Observability](adr/0008-observability.md)
  - [0009 — Accounts admin](adr/0009-accounts-admin.md)
  - [0010 — Database handle & read-replica routing](adr/0010-database-handle-and-read-replica-routing.md)
  - [0011 — Multi-backend & production roadmap](adr/0011-multi-backend-and-production-roadmap.md)
  - [0012 — Native CQRS](adr/0012-native-cqrs.md)
  - [0013 — prefetch (reverse relation, no N+1)](adr/0013-prefetch-related.md)
  - [0014 — Reversible migrations](adr/0014-reversible-migrations.md)
  - [0015 — Connection resilience](adr/0015-connection-resilience.md)
  - [0016 — Wider Value: float & bytes](adr/0016-wider-value-float-and-bytes.md)
  - [0017 — SQLite reader pool (WAL)](adr/0017-sqlite-reader-pool-wal.md)
  - [0018 — MySQL backend (plan)](adr/0018-mysql-backend.md)
  - [0019 — RAG (optional crate)](adr/0019-rag.md)
  - [0020 — MCP server (optional crate)](adr/0020-mcp-server.md)
  - [0021 — Tenant-scope enforcement (fail-closed)](adr/0021-tenant-scope-enforcement.md)
  - [0022 — Online migrations (rolling upgrades)](adr/0022-online-migrations.md)
  - [0023 — Optimistic concurrency & safe set_local](adr/0023-optimistic-concurrency.md)
  - [0024 — Keyset pagination (signed cursor)](adr/0024-keyset-pagination.md)
  - [0025 — Extended type system (net, enum, array, JSONB, composite PK)](adr/0025-extended-type-system.md)
