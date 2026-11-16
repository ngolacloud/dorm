# Security Policy

DORM is a typed ORM: application code describes models and filters, and DORM turns them into SQL
that runs against your PostgreSQL, SQLite or MySQL database — often carrying the credentials and
the multi-tenant isolation of a production system. A vulnerability here isn't "a bug" — it can mean
SQL injection through a path that looked parameterized, a tenant reading another tenant's rows, or
credentials and query data leaking where they shouldn't. We take reports in this category
seriously and ask that you do too, by not disclosing them publicly before a fix is available.

## Reporting a vulnerability

**Please do not open a public GitHub issue for security vulnerabilities.**

Use **[GitHub Private Vulnerability Reporting](https://github.com/ngolacloud/dorm/security/advisories/new)**
(enabled on this repository) — go to the **Security** tab → **Report a vulnerability**. This opens
a private draft advisory visible only to you and the maintainers, with no public trace until it's
resolved and you both agree to disclose. If you can't use that channel, email
**security@ngolacloud.ao**.

Include, if you can:
- The affected crate(s) and version(s) (e.g. `dorm-core 0.6.1`, `dorm-cli 0.13.0`) and the backend
  (PostgreSQL / SQLite / MySQL)
- A minimal reproduction — the model, filter, migration, or query-builder call that triggers it,
  and ideally the SQL DORM emitted
- The concrete impact — what an attacker gains (data they can read/write, a boundary they cross),
  not just "this looks unsafe"
- Whether it's reachable from untrusted input (a filter string, a value, a column/relation name
  derived from a request) vs. requiring the attacker to already control the application's own code

## What's in scope

- **SQL injection** — any input that reaches emitted SQL unescaped: `filter_str` keys and values,
  the query builder, ordering/pagination/limit handling, raw-SQL escape hatches, or identifiers
  (table/column/relation names) that aren't properly quoted
- **Tenant / scope isolation bypass** — a query that returns or mutates rows outside its declared
  tenant/scope, or a way to drop the mandatory scope so a tenant reads another tenant's data
- **Migration safety** — SQL injection or unintended destructive statements generated from
  attacker-influenced schema or migration input
- **Credential and connection exposure** — DSNs, passwords or TLS material leaked through logs,
  error messages, panics, or the REPL/`dbshell`/`studio` surfaces; TLS downgrade or verification
  bypass on the database connection
- **Data exposure through logging** — query parameters or row data written to logs/telemetry where
  the policy says they shouldn't be
- **Auth/authorization bypass on the exposed surfaces** — `dorm-mcp` (database access over MCP),
  `dorm-rag`, `dorm studio` (GUI) or any server surface that accepts input from outside the trusted
  application process
- Memory-safety issues in any `unsafe` block that aren't already covered by Rust's guarantees

## What's out of scope

- SQL you are already authorized to run — if the application code can issue arbitrary queries,
  DORM faithfully running them is not a vulnerability
- Denial of service from a query you control (an expensive or unbounded query you wrote yourself);
  statement timeouts and limits are correctness/robustness features, not a security boundary
- Vulnerabilities in the database engine itself (PostgreSQL, SQLite, MySQL), in the driver
  (`tokio-postgres`, etc.), or in base images you choose to run
- Missing hardening best-practices with no concrete exploit path

If you're not sure whether something qualifies, report it privately anyway — worst case we
downgrade it to a normal issue together.

## Our process

1. We acknowledge new reports as promptly as we can.
2. We confirm the issue, assess severity, and work on a fix in the private advisory.
3. We coordinate a disclosure timeline with you — we default to fixing and releasing before public
   disclosure, and crediting you in the advisory and release notes unless you'd rather stay
   anonymous.
4. Once a fix ships, the advisory (and a CVE, where warranted) is published, and the affected
   crates are released with the fix.
