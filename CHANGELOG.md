# Changelog

All notable changes to **DORM** are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crates follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

DORM is a Cargo **workspace**: its crates version **independently** and publish on
their own cadence (see the three groups — core & tooling, optional ports, and
example/internal — in [README.md](README.md#license)). This file therefore tracks
changes **per crate**, and each entry names the crate and version it shipped in.

The workspace itself is at `0.2.0`.

## [Unreleased]

### Added

- Open-source governance pack: `CODE_OF_CONDUCT.md` (Contributor Covenant 2.1),
  `SECURITY.md` (responsible-disclosure policy for the ORM surface — SQL injection,
  tenant/scope isolation, credential and data exposure), this `CHANGELOG.md`, and
  `.github/` issue and pull-request templates.

## Baseline — current published versions

These are the versions each crate currently sits at. Pre-1.0 crates may still make
breaking changes on a minor bump, per SemVer `0.y.z`. Prose release notes for
individual crates, where they exist, live alongside their ADRs in `docs/`.

### Core & tooling (published to crates.io)

- `dorm-core` **0.6.1** — the typed query engine, registry and `filter_str` resolution.
- `dorm-derive` **0.3.0** — derive macros for models and schema.
- `dorm-migrations` **0.7.1** — migration authoring and apply.
- `dorm-schema` **0.3.1** — schema introspection and representation.
- `dorm-sqlite` **0.2.1** — SQLite backend.
- `dorm-cli` **0.13.0** — the `dorm` command line (migrate, studio, db shell, pull).
- `dorm-studio` **0.1.1** — the `dorm studio` GUI.

### Optional ports (published, own pre-1.0 cadence)

- `dorm-outbox` **0.1.0** — transactional outbox (ADR 0026).
- `dorm-jobs` **0.1.0** — job queue (ADR 0027).
- `dorm-eventstore` **0.1.0** — event store (ADR 0039).
- `dorm-rag` **0.1.0** — retrieval-augmented generation support (ADR 0019).
- `dorm-mcp` **0.1.0** — Model Context Protocol server over the database (ADR 0020).

### Example / internal (unpublished)

- `dorm-models`, `dorm-example`, `dorm-shell`, `dorm-messaging` — track the workspace
  version and are not released to crates.io.

[Unreleased]: https://github.com/ngolacloud/dorm/compare/main...HEAD
