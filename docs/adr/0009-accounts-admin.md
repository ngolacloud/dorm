# ADR 0009 — Application-account administration: `createsuperuser`, `changepassword`, `user …`

## Status

Accepted. CLI brick (this ADR): `dorm createsuperuser`, `dorm changepassword
<user>` and `dorm user list|create|delete` — native commands over a conventional
`orm_accounts` table, Argon2id password hashes in PHC form, no project build.
Proven live against a local PostgreSQL (see *The proof*). The pluggable
*accounts adapter* the roadmap sketches — the application owning its own `User`
model, login identifier and policy — is explicitly **not** this brick; see *What
this is not*.

## Context

The roadmap (Phase 5) promises Django-shaped account administration:
`createsuperuser`, `changepassword`, `user …`, with no-echo password entry,
DB-enforced uniqueness, transactional create/update, and non-interactive
operation over secure channels. It frames the end state as an **optional adapter
crate**: the core ORM imposes no `User` table, and a mature project defines its
own model, hashing service and policy, which the CLI then drives — "never writing
incompatible hashes directly."

That adapter is a large, open-ended piece: it needs a stable way for a *compiled*
project to expose its auth logic to the installed `dorm` binary, which — as the
CLI contract already explains for `validate`/`migrate`/`shell` — the native tool
cannot reach without building the project. Shipping nothing until that exists
would leave the single most-reached-for admin command (stand up the first admin
on a fresh database) unavailable, exactly when there is no application running yet
to ask.

So this ADR ships the **native default**: a built-in, conventional accounts store
that works the instant a DSN is configured, chosen so it is a strict *subset* of
the eventual adapter rather than a throwaway. The design question was the one the
roadmap names: how to administer accounts **without ever writing a hash a real
verifier cannot read back.**

## Decision

### A conventional table, created on demand — `orm_accounts`

Accounts live in one table the CLI owns by convention, created idempotently with
`CREATE TABLE IF NOT EXISTS` at the start of every account command — the same
pattern as the migration ledger, so a fresh database needs no separate migration
step:

| Column          | Type          | Notes                                              |
|-----------------|---------------|----------------------------------------------------|
| `id`            | `bigserial`   | Surrogate primary key.                             |
| `username`      | `text`        | `NOT NULL UNIQUE` — the login identifier.          |
| `email`         | `text`        | Optional; left `NULL` when not given.              |
| `password_hash` | `text`        | `NOT NULL` — the Argon2id PHC string, never plaintext. |
| `is_superuser`  | `boolean`     | `NOT NULL DEFAULT false`.                          |
| `is_active`     | `boolean`     | `NOT NULL DEFAULT true`.                           |
| `created_at`    | `timestamptz` | `NOT NULL DEFAULT now()`.                          |
| `updated_at`    | `timestamptz` | `NOT NULL DEFAULT now()`; set on `changepassword`. |

The `orm_` prefix keeps the table clear of a project's own tables. Uniqueness is
**DB-enforced**, not checked in Rust: a duplicate `username` is rejected by the
unique constraint, and the command turns that one PostgreSQL error into a human
message rather than racing a `SELECT`-then-`INSERT`.

### Argon2id, stored as a PHC string — the "never incompatible" guarantee

A password is hashed with Argon2id (the `argon2` crate's recommended defaults)
and stored as its **PHC string**:

```
$argon2id$v=19$m=19456,t=2,p=1$<base64 salt>$<base64 hash>
```

This is the whole answer to the roadmap's "never writes incompatible hashes." The
stored value is **self-describing**: the algorithm (`argon2id`), the version
(`v=19`) and the cost parameters (`m`, `t`, `p`) travel *inside* the string,
followed by a per-row random salt drawn from the OS CSPRNG. Any Argon2 verifier
in any language — including a future application adapter — parses those fields
from the stored value and verifies against them, with no out-of-band agreement on
"which algorithm, which cost." A bare hex digest would be the incompatible thing
the roadmap warns against; a PHC string is the opposite, incompatibility named out
of existence by the format.

Plaintext is never stored and never logged: it lives in one local variable for
the length of the hash call. ADR 0008 already guarantees the write's SQL carries
`$n` placeholders and the bound value is never handed to the tracing layer, so the
hash — let alone the password — cannot reach a log through `db`.

### Every value bound, exactly like ADR 0005

The account writes obey the one invariant that makes DORM injection-proof: **no
value is ever formatted into SQL.** The only identifiers in these statements are
the `&'static` column names written in the source; every username, email and
hash travels as `$1`, `$2`, … through `db::sql` / `db::sql_execute`. The sole
piece of SQL *chosen* at runtime is whether the `INSERT` names the `email`
column — and both arms are static strings, picked by whether an email was given
(absent → the column is omitted and the row's `email` is `NULL`, since `Value`
has no `NULL` variant). The writes also pass through `db::guard_write`, so a
read-only session refuses them with the same `Error::ReadOnly` every compiled
write gets.

### No-echo entry, with non-interactive escapes for scripts

Passwords are read in priority order:

1. `--password <PW>` — scriptable, but visible to `ps` and shell history; the
   help says so, and it is the wrong choice for anything real.
2. `--password-stdin` — one line read from stdin (Django's non-interactive form),
   trailing newline stripped. The right choice for pipelines, CI and tests.
3. an interactive no-echo prompt (`rpassword`), confirmed twice on create.

An empty password is refused. When no source is given **and** stdin is not a
terminal, the command fails with a message naming the three ways rather than
hanging on a prompt nobody can answer — the roadmap's "non-interactive operation
over secure channels." `user delete` is symmetric: it confirms in a terminal, and
requires `--yes` when stdin is not one, rather than deleting on an unanswerable
prompt.

### `user list` is a read

`user list` honours the global `--json` and **never prints `password_hash`** — it
projects `id, username, email, is_superuser, is_active, created_at`. The other
verbs print a one-line confirmation, or a stable JSON object under `--json`.

## The proof

Against a disposable local database:

```bash
createdb dorm_accounts_proof
export DELONIX_ORM_DSN="host=/var/run/postgresql user=$USER dbname=dorm_accounts_proof"

dorm createsuperuser --username admin --email admin@example.com --password secret
# → created superuser `admin` (id 1)

psql "$DELONIX_ORM_DSN" -tAc "SELECT password_hash FROM orm_accounts WHERE username='admin'"
# → $argon2id$v=19$m=19456,t=2,p=1$sTzCxt9OTfFpXcJO9/NNbQ$uDcqg0a1Ft/R1g4a+RhVlsZs/r8PC0LQv5aHrhIosx8

printf 'newsecret\n' | dorm changepassword admin --password-stdin
# → password changed for `admin`   (the stored hash changes; a fresh salt means a new string)

dorm createsuperuser --username admin --password x
# → error: a user named `admin` already exists   (exit 1 — the unique constraint)

dorm user create --username alice --email alice@example.com --password pw
dorm --json user list        # no password_hash in the output
dorm user delete alice --yes

dropdb dorm_accounts_proof
```

Verification was proven end-to-end with an Argon2 verifier: after
`changepassword`, the stored PHC string verifies the **new** password
(`newsecret`) and rejects the **old** one (`secret`) — the hash is not merely
different, it is a correct hash of the new secret. Because verification is just
`PasswordHash::new(stored)` + `Argon2::verify_password`, any consumer — the
application's login path, or a future adapter — reads the stored value with no
further agreement.

## What this is not

- **Not the pluggable accounts adapter.** The roadmap's end state is the
  application owning its `User` model, login identifier, required fields, password
  policy, roles, auditing and session-revocation — the CLI driving *that* logic
  through an authorised adapter. That needs a contract by which a compiled project
  exposes its auth to the native binary (the `validate`/`migrate`/`shell` cargo
  lane, extended), and is its own brick. This ADR is the **native default** that
  works before any such adapter exists; it is deliberately a subset of it, not a
  competing convention. When an adapter is configured, these commands should route
  to it instead of `orm_accounts` — a later ADR.
- **Not a password policy.** No length/complexity/breach rules, no rotation, no
  lockout. The only rule enforced is "not empty." Policy belongs to the
  application that owns the login, and the eventual adapter is where it lands.
- **Not login or sessions.** DORM administers the rows; it does not authenticate
  anyone, issue sessions or revoke them. The roadmap's "last administrator" and
  "inactive account" behaviours are policy that rides with the adapter — the
  `is_active`/`is_superuser` columns are present so that policy has something to
  read, but the CLI enforces none of it yet.
- **Not PostgreSQL-plus-SQLite.** Like the other native `db` commands (ADR 0005's
  `db execute`, the `information_schema` browsers), these render and run against
  PostgreSQL. SQLite is the development backend (ADR 0002); a later brick can add
  it symmetrically.

## Consequences

- A fresh PostgreSQL gets its first administrator with one native command and no
  project build — the command most needed when there is no application yet to ask.
- Every stored credential is a portable, self-describing Argon2id PHC string:
  compatible by construction with any Argon2 verifier, including the application
  adapter that will eventually supersede this store. No plaintext is ever written
  or logged.
- The account writes carry the ADR 0005 injection invariant and the ADR 0008
  no-secret-in-logs invariant unchanged — bound parameters, static identifiers,
  values never handed to the tracing layer.
- The CLI contract's `createsuperuser` / `changepassword` / `user …` row moves to
  **done** for the native default, with the adapter path named as the remaining
  work rather than silently implied.

## Reproducing

```bash
createdb dorm_accounts_proof
DELONIX_ORM_DSN="host=/var/run/postgresql user=$USER dbname=dorm_accounts_proof" \
  dorm createsuperuser --username admin --password secret
# inspect the hash; it starts with $argon2id$
dropdb dorm_accounts_proof
```
