# verve

A coding agent that runs in your terminal, against a model on your own machine. No account,
no request leaving the box, no editor to adopt.

```sh
verve --model qwen3-coder:latest --root .
```

It speaks the OpenAI-compatible API, so it works against [LLMuse](https://github.com/loken-ai/loken)
or anything else that serves one - the default backend is `http://127.0.0.1:11435`.

## What it does

Reads, searches and edits files under one root, runs commands, and talks to a language server
for the things a language server is better at than a grep. The loop is bounded: `--max-steps`
caps how far it will go on a single turn before handing control back.

## Where it is careful

- **One root, and it is enforced.** Every path-taking tool is confined under `--root`. A tool
  call that walks outside it is refused rather than resolved. A file may be created in
  directories that do not exist yet; the containment check applies to where the write would
  actually land, not to how the path was spelled.
- **Edits are approved, not applied.** A change arrives as a diff to accept or decline, and
  an agreement is remembered against *what you were shown* - this command, writes under this
  directory, requests to this host - not against the tool that showed it. The modal says what
  `[A]lways` will cover before you press it.
- **A patch is one change or none.** Every file in it is resolved and applied in memory before
  you are asked, and a failure part-way through the commit puts back what was already written.
- **A model's tool arguments are repaired before they are trusted.** They arrive slightly
  wrong often enough that guessing is not acceptable: the arguments are validated against a
  schema, repaired where the repair is unambiguous, and rejected where it is not. `--no-grammar`
  turns off the server-side constraint to exercise that path on purpose.
- **Egress is confined to public addresses.** `web_fetch` resolves a host rather than trusting
  the name, follows redirects itself, and re-checks every hop - so an approved public URL
  cannot bounce onto loopback or a cloud metadata endpoint. `--allow-host` opens a named
  exception for a service on your own network.

Read [SECURITY.md](SECURITY.md) before pointing this at anything you care about. In
particular, an approved `bash` command is not confined by the root: the sandbox covers the
tools that take paths, and a shell does not.

## Install

```sh
cargo install --path .     # or: cargo build --release  → target/release/verve
```

Needs Rust 1.82 or later, and a Unix-like system - the shell tool runs `/bin/sh`.

Two things it uses if they are present, and says so if they are not:

- **ripgrep** (`rg`) for the `grep` tool.
- **A language server** for `lsp_diagnostics` and `lsp_hover`: `rust-analyzer`,
  `pyright-langserver`, or `typescript-language-server`. If one is on your `PATH` under the
  usual name but does not work - a `rustup` shim for a component that was never installed
  answers to `rust-analyzer` and fails - point at a working one with `VERVE_LSP_RUST`,
  `VERVE_LSP_PYTHON`, `VERVE_LSP_TYPESCRIPT` or `VERVE_LSP_JAVASCRIPT`.

## Using it

| Key | |
|---|---|
| `Enter` | send |
| `Shift-Enter` | newline |
| `Esc` | interrupt the running turn |
| `PageUp` / `PageDown` | scroll |
| `Ctrl-C`, `Ctrl-Q` | quit |

At an approval prompt: `y` accepts once, `a` accepts everything in the stated scope for this
session, `n` or `Esc` declines. `Enter` deliberately does nothing there - it is the send key,
and a prompt appearing mid-sentence should not be able to approve a command you have not read.

## Sessions

Every session is written to `$XDG_DATA_HOME/verve/sessions/<id>.jsonl` as it happens, which
is what makes a bad run answerable afterwards.

```sh
verve --continue              # the most recent session for this root
verve --resume <id>           # a particular one
verve --no-transcript         # record nothing
```

A resumed session keeps its id and appends to the file it came from. It also remembers the
model, so `--model` becomes optional.

Logs go to `$XDG_STATE_HOME/verve/verve.log`, never to the terminal - the interface owns that
screen. `RUST_LOG=verve=debug` turns the volume up; `--log-file` puts it somewhere else.

## Configuring it

Settings come from four places. The narrower statement wins, because it is the one made
knowing more: a flag beats the environment, the environment beats `<root>/.verve.toml`, that
beats `~/.config/verve/config.toml`, and that beats the built-in default.

```toml
# ~/.config/verve/config.toml, or .verve.toml in a project
model = "qwen3-coder:latest"
tool-protocol = "auto"          # auto | native | envelope

[backend]
url = "http://127.0.0.1:11435"
api-key-env = "MY_BACKEND_KEY"  # name a variable; do not write the key here
headers = { "X-Team" = "verve" }

[limits]
max-steps = 16
context-tokens = 8192
max-answer-tokens = 4096

[web]
allow-hosts = ["docs.example.com"]

[session]
transcripts = true
```

`verve --show-config` prints which files were read and what everything resolved to. It names
headers and never prints their values.

**Credentials.** `VERVE_API_KEY` is checked first, then the variable named by `api-key-env`,
then an `api-key` written in the file. The last is accepted only if nobody else can read the
file - a secret in a world-readable file is not a secret. A key about to cross a network
unencrypted is warned about.

Everything also has an environment variable: `VERVE_MODEL`, `VERVE_BACKEND_URL`,
`VERVE_LOG_FILE`, `VERVE_API_KEY`.

## How tools are called

Two protocols, and `--tool-protocol auto` picks between them.

**Native** uses the server's own `tools` field and the `tool_calls` it returns, so results are
answered against ids the server actually issued. **Envelope** has the model emit one JSON
object per turn, constrained by a grammar generated from the tool schemas; it needs nothing
from the server beyond a completion, which is why it is what a small local model behind a
plain backend still works against.

`auto` starts native and drops to the envelope the first time a backend refuses the field,
finishing the turn either way. A backend that *accepts* `tools` and then ignores it cannot be
detected - it looks exactly like a model choosing not to call anything - so pin
`--tool-protocol envelope` for one of those.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

`cargo run --example smoke -- --model <m> --prompt "…"` drives one turn headlessly against a
real backend, with approvals stubbed.

See [CONTRIBUTING.md](CONTRIBUTING.md).

## Licence

MIT OR Apache-2.0, at your option.
