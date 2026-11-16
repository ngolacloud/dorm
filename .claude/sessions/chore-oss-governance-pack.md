---
session: 964ba859-8d98-4bc3-ace6-42f3f424eb5f
opened: 2026-11-16T13:29:07+01:00
branch: chore/oss-governance-pack
status: open
---

# Objective
Dar ao DORM o pacote de governança OSS em falta (CoC, SECURITY, CHANGELOG, templates .github), adaptado do Delonix e reescrito para o domínio de um ORM.

# Done when
- [x] `ls` confirma na raiz: CODE_OF_CONDUCT.md (Covenant, contacto do projecto), SECURITY.md (disclosure adaptado ao ORM — SQL/injecção/isolamento de inquilino/dados), CHANGELOG.md (Keep a Changelog, versões per-crate actuais) e .github/ com ISSUE_TEMPLATE/ + PULL_REQUEST_TEMPLATE.md
- [x] CODE_OF_CONDUCT.md e SECURITY.md estão ligados a partir do README.md
- [x] `cargo fmt --all --check` continua verde (os ficheiros são markdown/config, não alteram código)

# Out of scope
- GOVERNANCE.md, MAINTAINERS.md, CODEOWNERS (não essenciais no 1º corte)
- git push e merge de PR (são do dono)
- Reescrever CONTRIBUTING.md ou workflows de CI existentes

# Handoff
(pending)
