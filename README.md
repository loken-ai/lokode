# verve

A coding agent that runs in your terminal, against a model on your own machine. No account,
no request leaving the box, no editor to adopt.

```sh
verve --model qwen3-coder:latest --root .
```

It speaks the OpenAI-compatible API, so it works against [loken](https://github.com/loken-ai/loken),
ollama, vLLM, llama.cpp or anything else that serves one - the default backend is
`http://127.0.0.1:11435`. It also speaks the Anthropic Messages API, for the days you want a
model that is not on your machine.

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
particular, an approved `bash` command is not confined by the root by default: the sandbox
covers the tools that take paths, and a shell does not. On Linux, `--confine-shell` makes
everything outside the project read-only for it.

## Install

```sh
cargo install --path .     # or: cargo build --release  → target/release/verve
```

Needs Rust 1.82 or later, and a Unix-like system - the shell tool runs `/bin/sh`.

One thing it uses if it is there, and one it needs:

- **ripgrep** (`rg`) makes `grep` faster and is not required - without it the same search
  runs on the built-in walker, honouring the same ignore files.
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
| `↑` / `↓` | walk what you have already sent (from an empty input) |
| `PageUp` / `PageDown`, wheel | scroll |
| `Ctrl-C`, `Ctrl-Q` | quit |

The model's prose is rendered as the light markdown it tends to write - fenced code,
headings, `code` and **bold** - and anything that is a diff is coloured as one.

The status line says what the turn is doing whenever nothing is being printed - fitting the
history, waiting for the model, thinking, answering, repairing a reply, running a named
tool - with how long it has been doing it, beside a spinner that moves while a turn is
alive. Most of a turn is spent in those phases: the request has gone out and the model is
reading a context of thousands of tokens, or a command is running under a tool. Without
them the screen is identical to one that has stopped.

At an approval prompt: `y` accepts once, `a` accepts everything in the stated scope for this
session, `n` or `Esc` declines. `Enter` deliberately does nothing there - it is the send key,
and a prompt appearing mid-sentence should not be able to approve a command you have not read.

## What the project tells the agent

`.verve.md` at the root of a project, and `~/.config/verve/verve.md` for yourself, are read
at the start of every session and appended to the system message: the conventions, the
command that checks a change, what must not be touched. The user's file comes first and the
project's second, each named, so the model can tell whose rule it is reading.

`/init` writes a first draft from what the repository shows - the language, the test
command, the layout - for you to edit. Keep it short: this text is in the system message,
which compaction never drops, so it holds the window for the whole session. What does not
fit is trimmed at a line boundary and said out loud, because an instruction the agent never
received would otherwise look like one it ignored.

## Taking something back

Every change the agent makes is committed to a git repository kept beside the session, with
the project as its work tree. Your own repository is not touched: different directory, and
excluded from this one.

```
/diff    everything the agent has changed this session
/undo    put back the most recent turn that changed something
/help
```

`/undo` again goes one turn further back. It never rewinds - the undo is itself a commit
carrying the older state - so a file you edited by hand between the agent's change and your
decision to take it back is still in that history rather than discarded. See
[SECURITY.md](SECURITY.md): the history holds copies of your files and outlives the session.

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

## Without the interface

`-p` runs one turn and exits, for a git hook, a pipeline, or anything else with nobody sitting
in front of it.

```sh
verve -p "fix the clippy warnings" --approve edits --output json
```

Since there is nobody to answer an approval, the policy is stated up front. `--approve none`
is the default and declines everything that would change anything - the agent can still read
and search. `edits` allows writes, edits and patches, and not the shell, the test suite or the
network. `all` allows everything. Each call is judged on its own; a policy never grants a
standing agreement.

`--output text` streams the model's prose to stdout and progress to stderr, so a caller can
take the answer and discard the narration. `--output json` writes one object at the end, with
the result, every tool call and its output, and whether anything was refused.

The exit code says what happened without anything having to be parsed: `0` finished, `1`
something failed - a backend error, or the step limit - and `2` a tool was refused by the
policy, which is a different thing from the model deciding not to use one.

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
kind = "auto"                   # auto | openai | anthropic
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

## Tools from elsewhere

A Model Context Protocol server is a program you configure; whatever it offers becomes tools
the model can call, named `<server>__<tool>` so nothing can take the name of a built-in.

```toml
[mcp.servers.github]
command = "mcp-server-github"
args = []
env = { GITHUB_TOKEN = "…" }     # prefer naming a variable your shell already has
```

They are not trusted the way the built-in tools are. A built-in is trusted because its
behaviour is in this repository; one of these is somebody else's program, and the root
confines it no more than it confines a shell. So every call is approved, every call is
treated as able to change the project, and the schema a server advertises is checked before
it is allowed to shape the grammar the model is held to - a schema this code cannot compile
would not break one tool, it would break the turn. A tool that fails that check is refused by
name and said so, and a server that will not start takes only itself down.

## Which API

`kind = "auto"` reads it off the URL and is right for every ordinary case: `api.anthropic.com`
is the Messages API, everything else is OpenAI-compatible. Name one explicitly to reach a
proxy that speaks Anthropic's format on your own host.

The credential is resolved the same way either way — the environment, then a named variable,
then a file only you can read. Only where it goes on the wire differs: `Authorization: Bearer`
for the OpenAI family, `x-api-key` for Anthropic. Converting a working configuration between
them is a header change, not a value change, and verve makes it for you.

What differs beyond that is absorbed at the boundary. Anthropic's system prompt is a top-level
field rather than a message, its tool results are blocks inside a user turn rather than their
own role, and its stream speaks a different alphabet. None of that reaches the loop, the
transcript or the screen. Reasoning is the same story from the other direction: `<think>` tags
inline, a `reasoning` field, a `reasoning_content` field and Anthropic's thinking blocks are
four spellings of one thing, and verve shows all four the same way — set apart, and never fed
back to the model.

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

See [CONTRIBUTING.md](CONTRIBUTING.md).

## Licence

MIT OR Apache-2.0, at your option.
