# Contributing

## Getting it running

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

CI runs exactly those, on Linux and macOS, plus `cargo deny check` and a build against the declared MSRV.  
Nothing lands red.

`LOKODE_SITE_DOCS` set to the folder of the guides in the site repository makes `tests/docs.rs` check that every flag and tool is documented.

Every tool has tests.
The ones that need another program skip themselves when it is missing, so a clean checkout passes.  
`run_tests` wants a project manifest.

The LSP tools speak to a stub language server the test writes out in Python.
That stub reports the document version back inside its diagnostic, which tells a fresh answer from a stale one.

`lokode -p "…" --output json` drives one turn against a real backend without the interface, which is the quickest way to see a change end to end.  
`src/boot.rs` assembles the agent for both that and the terminal.

## What the code is trying to be

The comments explain *why*, not *what*.  
`src/agent/session.rs` and `src/sandbox/allowlist.rs` are the house style: each says which failure the code exists to prevent, so that someone changing it later knows what they would be giving up.

A comment restating the line below it is worse than none.

Commit messages give the problem, then the resolution.
`git log` is the design record.

## The parts to be careful with

**`src/sandbox/allowlist.rs`** is the only thing standing between the model and the rest of the filesystem.  
Its tests assert on consequences rather than on the shape of a path: the dangling-symlink case writes through the resolved path and checks nothing appeared outside.

Keep new tests in that form: a path that merely *looks* wrong is not the failure.

**`src/sandbox/approval.rs`** decides what an `[A]lways` covers.
Widening a scope is a security change even when it reads like a convenience.  
If a prompt cannot state its scope in a short phrase, the scope is probably wrong.

**`src/agent/session.rs`** keeps the conversation inside the context window.
The system message carries the tool manifest, a server that has to truncate keeps the tail, and so letting the history overflow is what silently removes the agent's knowledge of its own tools.

## The ways in

The terminal screen, `lokode acp` and `lokode serve` are 3 views of one engine, and `lokode attach` is a client of `lokode serve`.

The events the engine sends are turned into what an editor reads in `src/acp/updates.rs`, and back into what the screen draws in `src/ui/remote.rs`.

The page of `lokode serve` is plain HTML, CSS and JavaScript in `src/serve/`, embedded in the binary, with no build step and no dependency.

A change to what the engine says to its interface touches all 4.
`AgentEvent` is where to start, and a match that does not name a new event is a bug the compiler will not find for you.

## Plugins from other agents

`src/claude.rs` is the one place that knows another agent's names: the tools in a matcher, the events of a hook, the fields a hook script reads.

Keep `src/claude.rs` so, and say in the plugin's listing what lokode leaves out rather than skipping it.

## Adding a tool

One file in `src/tools/`, one `impl Tool`, one line in `build_registry`.  
The JSON schema, the grammar the model is held to and the manifest in the system prompt are all generated from `args_schema()`, so the loop does not change.

If it can change something, reach the network or run code, it must go through `ctx.approve(...)` with a scope that describes what it is about to do.

If the tool can wait, it must race `ctx.cancel` so `Esc` reaches it.  
If it produces output, clamp it with `ctx.clamp`.

Then tests: a nominal case and a refusal, using `src/tools/harness.rs`, which gives you a root in a temporary directory and answers approvals however the case wants.

## The site

The documentation is built into the site of loken-ai, at https://loken-ai.github.io/lokode/, from the repository `loken-ai/loken-ai.github.io`.  
It takes the pages of the latest release of lokode.

The guides in `docs/guides` stay readable on GitHub as they are.

## Reporting things

Bugs and features: https://github.com/loken-ai/lokode/issues.  
Security: [SECURITY.md](SECURITY.md).

## Licence

Contributions are under MIT OR Apache-2.0, matching the project.
