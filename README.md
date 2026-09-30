# dex-cli

The DEX terminal frontend.

This repository is presentation only. It opens a Unix socket, sends user
messages, and renders the structured events that come back. It never reads,
writes, searches, or executes anything in a repository, and it never starts the
runtime.

That is a deliberate constraint rather than a limitation. If the CLI could
execute a repository operation directly, the boundary the project exists to
demonstrate would be untestable. Everything the user sees on screen is an event
the runtime emitted.

## Running

Start the runtime first:

```bash
# in a checkout of dex-runtime
cargo run --release --bin dexd
```

Then:

```bash
dex                          # interactive
dex -p "find the auth code"  # one turn, then exit
```

If the socket is missing, `dex` says so. It does not quietly spawn a runtime,
because a hidden spawn would hide the process boundary the two-repository split
is meant to prove.

## Protocol

`dex-protocol` is consumed as a git dependency from
[dex-harness/dex](https://github.com/dex-harness/dex.git), so the wire format
has exactly one definition. For local development, `.cargo/config.toml`
substitutes the sibling checkout; delete that file to build against the
published commit.

The IPC path is binary: a `u32` little-endian length prefix followed by a
`postcard`-encoded payload. `dex-protocol`'s golden fixtures pin the exact bytes
of every frame shape the CLI depends on, and this repository asserts the same
values, so an incompatible protocol change fails this build rather than
surfacing as a decode failure at runtime.

## Layout

```text
crates/
├── dex-client/   socket, frames, event stream. No presentation.
└── dex-cli/      bin `dex`: input, rendering, REPL.
```
