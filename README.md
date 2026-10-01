# dex-cli

The DEX terminal frontend.

This repository is presentation only. It opens a Unix socket, sends user
messages, and renders the structured events that come back. It never reads,
writes, searches or executes anything in a repository, and it never starts the
runtime.

That is a deliberate constraint rather than a limitation. If the CLI could
execute a repository operation directly, the boundary the project exists to
demonstrate would be untestable. Everything on screen is an event the runtime
emitted.

## Running it

Start the runtime first:

```bash
# in a checkout of dex-runtime
cp .env.example .env      # add an API key and a DEX_AUTHORITY grant
cargo run --release --bin dexd
```

Then:

```bash
dex                              # interactive, in this directory
dex --cwd /path/to/repo          # interactive, elsewhere
dex --cwd /path/to/repo -p "..." # one turn, then exit
dex --socket /tmp/dex.sock ...   # override the socket location
```

If the socket is missing, `dex` says so and stops. It does not quietly spawn a
runtime, because a hidden spawn would hide the process boundary the two-repository
split is meant to prove.

## What it renders

Everything comes from an event. Nothing is inferred and nothing is fetched: if
it is not on screen, the runtime did not say it.

```text
session kimi-k2.7-code in /work
> find where login is implemented

thinking...
program #0
  · repo.find "login"
  ✓ repo.find ok
  · repo.read "src/auth.rs"
  ✓ repo.read ok

login is in src/auth.rs at line 1
session done
```

Program source streams as the model writes it, so a long program is visible while
it is being produced rather than appearing all at once.

## Ctrl-C

The same key means two different things, and the difference is only meaningful
because a turn can be in flight:

- **during a turn** it asks the runtime to cancel, which reaches the model
  request, a running test process, and the executing program;
- **at the prompt** it quits, because there is nothing to cancel.

## Protocol

`dex-protocol` is consumed as a git dependency from
[dex-harness/dex](https://github.com/dex-harness/dex.git), so the wire format has
exactly one definition. For local development, `.cargo/config.toml` substitutes
the sibling checkout; delete that file to build against the published commit.

The IPC path is binary: a `u32` little-endian length prefix followed by a
`postcard`-encoded payload. `dex-protocol`'s golden fixtures pin the exact bytes
of every frame shape, and both repositories assert them, so an incompatible
protocol change fails a build rather than surfacing as a decode failure at
runtime.

The transport here is a deliberate copy of the runtime's rather than a shared
helper. The two repositories must not depend on each other; `dex-protocol` is
the vocabulary they share, and each side owns how bytes move.

## Layout

```text
crates/
├── dex-client/   socket, frames, event stream. No presentation.
└── dex-cli/      bin `dex`: input, rendering, REPL.
```

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
```
