# Contributing to the partiri CLI

Thanks for your interest in improving the Partiri CLI. This guide covers how to
build, test, and submit changes.

## Prerequisites

- **Rust 1.77 or newer** (edition 2021) — install via [rustup](https://rustup.rs).
- No system OpenSSL is required; TLS is handled by `rustls`.

## Building

```bash
cargo build            # debug build   → target/debug/partiri
cargo build --release  # release build → target/release/partiri
```

Run the CLI straight from source:

```bash
cargo run -- <command>
# e.g.
cargo run -- llm doctor
```

## Testing

```bash
cargo test             # full unit + property-based test suite
```

Please add or update tests for any behavior you change. The config model
(`src/config.rs`) and its validation logic are covered by unit and
property-based (`proptest`) tests — mirror that style for new code.

## Formatting and linting

Both are expected to pass before a pull request is merged:

```bash
cargo fmt --all --check      # standard rustfmt defaults
cargo clippy --all-targets   # resolve all warnings
```

## Commit messages

- Short, imperative descriptions ("Add worker deploy type", not "Added…").
- No conventional-commits prefix (`feat:`, `fix:`, …).
- One logical change per commit where practical.

## Pull requests

1. Fork the repository and create a feature branch.
2. Make your change with accompanying tests.
3. Ensure `cargo test`, `cargo fmt --all --check`, and `cargo clippy` all pass.
4. Open a PR describing **what** changed and **why**.

## Project layout

| Path            | Purpose                                                      |
|-----------------|--------------------------------------------------------------|
| `src/main.rs`   | Entry point and command dispatch                             |
| `src/cli.rs`    | `clap` command tree                                          |
| `src/client.rs` | Blocking HTTP client and API response types                  |
| `src/config.rs` | `.partiri.jsonc` model, (de)serialization, and validation    |
| `src/modules/`  | One submodule per command group                              |
| `LLM.md`        | Agent-facing guide, embedded in the binary (`partiri llm guide`) |
| `npm/`          | npm platform-package wrappers (binaries filled in during release) |

## Reporting security issues

Please do **not** open public issues for security vulnerabilities. See
[SECURITY.md](SECURITY.md) for the disclosure process.
