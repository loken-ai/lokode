# Contributing

## Getting it running

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

CI runs exactly those, on Linux and macOS, plus `cargo deny check` and a build against the
declared MSRV. Nothing lands red.

Every tool has tests. The ones that need another program skip themselves when it is missing,
so a clean checkout passes: `run_tests` wants a project manifest, and the LSP tools speak to a
stub language server the test writes out in Python. That stub reports the document version
back inside its diagnostic, which is the only way to tell a fresh answer from a stale one -
the bug it exists to catch was the server never being told a file had changed.

`lokode -p "…" --output json` drives one turn against a real backend without the interface,
which is the quickest way to see a change end to end. `src/boot.rs` assembles the agent for
both that and the terminal, so neither entry point can drift from the other.

## What the code is trying to be

The comments explain *why*, not *what*. `src/agent/session.rs` and `src/sandbox/allowlist.rs`
are the house style: each says which failure the code exists to prevent, so that someone
changing it later knows what they would be giving up. A comment restating the line below it is
worse than none.

Commit messages do the same at a larger scale - the problem, then the resolution. `git log`
is the design record for this project.

## The parts to be careful with

**`src/sandbox/allowlist.rs`** is the only thing standing between the model and the rest of
the filesystem. Its tests assert on consequences rather than on the shape of a path: the
dangling-symlink case writes through the resolved path and checks nothing appeared outside.
Keep new tests in that form - a path that merely *looks* wrong is not the failure.

**`src/sandbox/approval.rs`** decides what an `[A]lways` covers. Widening a scope is a
security change even when it reads like a convenience. If a prompt cannot state its scope in a
short phrase, the scope is probably wrong.

**`src/agent/session.rs`** keeps the conversation inside the context window. The system
message carries the tool manifest, a server that has to truncate keeps the tail, and so
letting the history overflow is what silently removes the agent's knowledge of its own tools.

## Adding a tool

One file in `src/tools/`, one `impl Tool`, one line in `build_registry`. The JSON schema, the
grammar the model is held to and the manifest in the system prompt are all generated from
`args_schema()`, so the loop does not change.

If it can change something, reach the network or run code, it must go through
`ctx.approve(...)` with a scope that describes what it is about to do. If it can wait, it must
race `ctx.cancel` so `Esc` reaches it. If it produces output, clamp it with `ctx.clamp`.

Then tests: a nominal case and a refusal, using `src/tools/harness.rs`, which gives you a root
in a temporary directory and answers approvals however the case wants.

## Reporting things

Bugs and features: https://github.com/loken-ai/lokode/issues.
Security: [SECURITY.md](SECURITY.md).

## Licence

Contributions are under MIT OR Apache-2.0, matching the project.
