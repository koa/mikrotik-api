# mikrotik-api

Low-level async Rust client for the native MikroTik RouterOS API protocol (binary sentence
protocol over TCP, default port 8728). No domain/config knowledge — pure transport layer.
Used by `mikrotik-model` (in this workspace: `../mikrotik-model`), which builds the typed
RouterOS config model on top of it.

A `Connection` is one login over one transport and runs commands of any type
(`Connection::run`): the dispatch task hands each reply as it came (`Reply`) to its command,
which reads it with its `Command::Reply`, so raw commands and typed reads share a login. It
never connects again; once the transport ends `is_closed()` is true. A `Command` (`command.rs`)
has its path, attributes, reply type and `Effect` as a type (`Read`, `Observe`, `Write`,
`SessionBound`), so a caller can take commands of one effect only; a `Write` or `SessionBound`
command advances the `Generation` given at the connect, which all connections of a device may
share (what was read before may be out of date). `RawCommand` is for commands without a type of
their own and a code smell: each is a command missing from mikrotik-model's catalog.

Login is the post-6.43 one (`/login =name= =password=`). A RouterOS before 6.43 answers it with a
challenge (`!done =ret=<hex>`), which `connect_with_stream` answers once with
`=response=00<md5(0x00 + password + challenge)>`; any other reply is `LoginFailed`.
When the other end closes the connection before answering the login (the device, or a proxy on
the way), the connect fails with `ClosedAtLogin`. A connection that ends later (closed, a read or
protocol error) answers its running commands, the ones still queued and every later one with
`ConnectionClosed`; `run` never panics on it.

Nothing waits for the device for good: the login has `LOGIN_TIMEOUT` (30 s, `LoginTimeout`), and
a command that gets no reply for `REPLY_TIMEOUT` (120 s) answers `ReplyTimeout` and is cancelled
on the device (`/cancel`); each reply starts the time again, so a monitor or countdown that keeps
answering goes on. Replies dropped before the end (a read taking the first row) cancel their
command the same way, at its next reply: RouterOS takes `=tag=<n>` there, `=tag==<n>`
cancels nothing (CHR 7.24.5, a ping went on to its end). A command that rightly stays silent
longer (a script that answers at its end) sets its own limit (`Command::reply_timeout`,
`None`: as long as the connection lasts). The dispatch task checks once a second;
`tests/timeouts.rs` runs on tokio's paused clock.

## Commits

- One focused commit per logical change; split unrelated work (e.g. a refactor and the feature
  built on it) into separate commits.
- Subject: short, lowercase, imperative, no trailing period (e.g. `print panel labels`).
- Body: why the change was needed and anything non-obvious, wrapped at ~72 columns.
- Update this file in the same commit as the change it describes.
- Commits by Claude end with the `Co-Authored-By: Claude …` trailer, without a session link.
- Format with `cargo fmt` before every commit; a formatting-only commit goes into
  `.git-blame-ignore-revs`.
- Push to `master` only when asked.
