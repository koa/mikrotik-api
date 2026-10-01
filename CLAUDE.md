# mikrotik-api

Low-level async Rust client for the native MikroTik RouterOS API protocol (binary sentence
protocol over TCP, default port 8728). No domain/config knowledge — pure transport layer.
Used by `mikrotik-model` (in this workspace: `../mikrotik-model`), which builds the typed
RouterOS config model on top of it.

## Commits

- One focused commit per logical change; split unrelated work (e.g. a refactor and the feature
  built on it) into separate commits.
- Subject: short, lowercase, imperative, no trailing period (e.g. `print panel labels`).
- Body: why the change was needed and anything non-obvious, wrapped at ~72 columns.
- Update this file in the same commit as the change it describes.
- Commits by Claude end with the `Co-Authored-By: Claude …` trailer, without a session link.
- Push to `master` only when asked.
