---
name: Bug report
about: Something doesn't work the way it should
title: ""
labels: bug
assignees: ""
---

**Affected crate(s) and version(s)**

```
(e.g. dorm-core 0.6.1, dorm-cli 0.13.0 — `dorm --version` for the CLI)
```

**Environment**
- Backend and version: PostgreSQL / SQLite / MySQL — which version?
- Rust toolchain (`rustc --version`):
- Installed from crates.io or built from source:

**What did you run?**

```
(the model definition, the filter / query-builder call, the migration, or the `dorm` command
that triggers this — a minimal reproduction is worth ten paragraphs)
```

**What SQL did DORM emit, if you can tell?**

```
(the generated statement, if relevant — this usually pinpoints the bug fast)
```

**What did you expect to happen?**

**What actually happened?**

```
(full output/error — do not trim it, the exact wording usually matters here)
```

**Can you reproduce it reliably?**
- [ ] Yes, every time
- [ ] Sometimes
- [ ] Only once, not sure how to reproduce

**Anything else that might be relevant?** (an unusual schema, a reverse relation, a custom type
mapping, a specific transaction/isolation setup, etc.)

---

If this looks like a security vulnerability (SQL injection, a tenant reading another tenant's data,
credential or query-data leakage) — please **don't** file it here. See
[SECURITY.md](../../SECURITY.md) for private disclosure instead.
