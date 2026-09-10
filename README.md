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
| `/plan`, `/act` | plan without changing anything, then carry it out |
| `Enter` while it works | hand the line to the turn in flight |
| `@path` in a prompt | send that file, or that directory's listing, with the question |
| `↑` / `↓` | walk what you have already sent (from an empty input) |
| `/retry`, `/retry <model>` | ask the last question again, of the same model or another |
| `PageUp` / `PageDown`, wheel | scroll |
| `Ctrl-C`, `Ctrl-Q` | quit |

The model's prose is rendered as the light markdown it tends to write - fenced code,
headings, `code` and **bold** - and anything that is a diff is coloured as one.

Typing while the agent works is allowed: the line joins the conversation at the next step
boundary and the screen says when it was handed over. That is the difference between
steering a turn and abandoning it. A slash command typed then runs immediately.

Naming a file with `@` sends it with the question: "why does `@src/main.rs` do nothing?"
arrives with the file rather than costing a round trip to fetch what you had already
decided was relevant. A directory sends its listing. What is attached is bounded like any
other thing that lands in the window, a mention that is not a path here is left alone, and
nothing outside the project can be reached by naming it.

A shell command and a test suite show their output while they run, as a dimmed tail that
the tool's own result replaces when it is over. A build that says nothing for two minutes
cannot be told from one that has hung, and the line naming the failing test is worth having
before the suite finishes. What reaches the model is still the whole of it.

The status line says what the turn is doing whenever nothing is being printed - fitting the
history, waiting for the model, thinking, answering, repairing a reply, running a named
tool - with how long it has been doing it, beside a spinner that moves while a turn is
alive. Most of a turn is spent in those phases: the request has gone out and the model is
reading a context of thousands of tokens, or a command is running under a tool. Without
them the screen is identical to one that has stopped.

While the model writes its answer the card is busy and the rest of the machine is not. A
small model spends that time guessing which read is coming next, the read is made, and the
step that asks for it is answered without a round trip. It is abandoned the moment the
answer arrives: a guess is worth having only if it lands first, and one that has not must
never be something the turn waits on. Read-only by construction too, so a guess that would
change something is dropped rather than run, because being wrong has to stay free. `speculate = true` turns it on, against a backend on this machine or this network. It is
off by default and should stay off unless the guessing model can stay resident beside the
one answering: where the cards hold only one, every guess evicts the model the turn is
waiting on and the feature costs what it was meant to save.

A turn closes with what it came to: how long it took, how many steps, what it spent and
which files it changed. A turn that answered in a moment and changed nothing says nothing,
because a receipt for that is noise.

Three things the loop declines to do twice. A read the model already made in this turn,
with nothing written since, is answered with what it returned and told it is repeating
itself. A whole-file write over a file that has changed on disk since the model read it is
refused, because that write would discard whatever you did in your own editor. And a path
that is not there is answered with where the file actually is, rather than with a refusal
that costs a search to act on.

`/retry` asks the last question again from where it was asked: what the agent did after it
leaves the conversation, so the model is not reading its own failed attempt while it tries
again. `/retry <model>` asks a different one, which is the cheapest experiment there is when
a turn goes wrong. The transcript keeps both attempts, because what was tried is part of
what happened, and `/undo` puts the files back if the first attempt wrote any.

A turn that is getting nowhere is handed back rather than run to the step limit. Three
steps in a row where every call either failed or had already been answered, or six failed
calls in a turn that has changed nothing, and the turn ends saying which tool kept failing.
A weak model looping on one read cost sixteen steps and twenty thousand tokens before this;
it now costs six steps and under four thousand.

At an approval prompt: `y` accepts once, `a` accepts everything in the stated scope for this
session, `n` or `Esc` declines. `Enter` deliberately does nothing there - it is the send key,
and a prompt appearing mid-sentence should not be able to approve a command you have not read.

## Working it out while you are not waiting

`--discover`, or `discover = true` in the configuration, gives the session a second thread
of work: while nothing else is running, one question about the project is taken off a
queue, answered by a read-only session of its own, and what it found is written down. The
queue and what has been learned both outlive the session, so the finding out is done once
rather than at the start of every day.

It is held in place by three rules. It runs only while nothing else does and stops the
instant you press Enter. It asks the server at a priority that yields, so the answer you
are waiting for is never behind it. And it plans rather than acts, which means every tool
that writes, runs a command or reaches the network refuses before it runs.

What it learns goes in front of the model next session, as a short paragraph of statements
rather than the dozen searches that produced them. That is the point: the searching is what
fills a window, and a window spent on rediscovery is a window not spent on the work.
`/digest` shows what is known and where each statement came from, `/digest forget` clears
it, and what is worth keeping belongs in `.verve.md` where the rest of the team gets it.

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

## Handing off a search

`delegate` gives one self-contained question a session of its own, with its own window and
the same tools, and brings back only what it answered. A search that reads six files to
settle one question otherwise spends the conversation's budget on text nobody needs
afterwards, and once compaction drops it what is left is the middle of a hunt with no
beginning.

The sub-session cannot delegate again, and its step limit is lower than a turn's: work that
needs more than a few steps was not self-contained, and saying so is more use than waiting
through it. It runs under the caller's approvals and the caller's cancellation, because it
is the same person's work.

## Work of several steps

`update_plan` is a tool the model calls to keep a task list: what is done, what is being
worked on, what is left. It appears as a pane above the input while there is a list, and
disappears when there is not. Nothing is written to disk and nothing is approved, since it
changes nothing outside the session.

It is for the person first: a step counter says how far a turn has gone and nothing about
what it is doing. It helps a small model too, whose window drops the oldest messages first
and with them the goal it was given.

## Changing a file

`write_file` replaces a file, `edit_file` replaces one unique substring, `edit_lines`
applies several of those to one file in order, all or none, and `apply_patch` takes a
unified diff across several files. Prefer `edit_lines` over repeated single edits: on a
local model each round trip is seconds, and an edit that cannot be placed leaves the file
as it was rather than half of two versions.

After a write, the language server is asked about that file and anything it reports comes
back in the same tool result. The model corrects its own syntax error on the spot instead
of learning about it a turn later, if it thinks to ask.

## Hooks

A project can put its own commands at the edges of the loop:

```toml
[hooks]
pre-tool = "./scripts/guard.sh"
post-tool = "cargo fmt"
turn-end = "cargo test --quiet"
```

They run only around a tool that changes something, because a hook on every read would run
on the searches an agent does by the dozen. A non-zero exit from the pre-tool hook refuses
the call and what it printed is handed to the model, which is the only form of guidance an
agent cannot skip. The post-tool hook's output is handed over too, since a formatter that
rewrote the file has something the model needs to know. The turn-end hook reports to you,
and runs for an abandoned turn as well, because that is exactly when files have been left
half-changed.

The hook is given the tool's name, the project root and which edge it is at, in the
environment. It is bounded in time and its output is bounded in length, since both land
inside a turn.

## Commands a project ships

A Markdown file under `.verve/commands` is a command: `review.md` makes `/review`, and what
it holds becomes that turn's prompt. Anything typed after the name replaces `$ARGS`, or is
appended when the file does not name it, so one command serves a family of tasks. `/help`
lists the project's alongside the built-ins.

That is how a team ships its way of working - a review checklist, a release routine -
without anyone writing Rust, and it is in version control with the code it is about.

## What it cost

`/cost` says what the session has spent: steps, tokens in and out, the rate the answers
arrived at, and how much of the time went to the model rather than to tools, then the last
few steps one by one. The server was already counting the tokens and nothing showed them,
so the question "was that worth it" had no answer.

Every step is also written to the transcript with what was knowable before it - the size of
the history, what the previous step called, whether its write came back clean, the mode -
and what it cost. Nothing routes on any of it yet. A claim that one arrangement of models
is faster than another needs both measured, and that needs the measurement to exist first.

## Where the window stands

The status line carries how full the history budget is, in per cent, amber past three
quarters and red at the line where the oldest steps start being elided. `/context` says
what is holding it - the standing instructions and the tool manifest, what you said, what
the model said, the tool results - largest first. `/compact` elides now rather than waiting
for the window to fill, which is what you want before handing the agent a large file.

Compaction announcing itself after the fact was the only signal there had ever been, and by
then the session had already forgotten something.

## Settling approvals in advance

An agreement given at the prompt lasts as long as the session, so working the same way
every day means answering the same questions every day. An `[approve]` section states them
once, in a file that can be read in review:

```toml
[approve]
max-auto = 50                      # per turn, then it asks again

[approve.allow]
bash = ["cargo test*", "git status"]
write = ["src/*"]
egress = ["docs.rs"]

[approve.deny]
bash = ["git push*", "*rm -rf*"]
```

A refusal wins over an agreement, always: a rule written down so that something never
happens must not be reachable by an agreement given in a hurry. Patterns take one wildcard,
`*`, for any run of characters, because a rule language with more in it invites a rule
nobody can read. Write rules are read relative to the project, so the same file works on
another machine. The cap is per turn: a standing agreement is for the work a person
expects, and a loop calling a tool a thousand times is not that work. `/approve` says what
is in force.

A run driven by a script does not take the file's word for what may go ahead: the policy on
its command line is the narrower statement and the one its author made for that run. The
file's refusals still apply, because nothing may widen one.

## Planning before acting

`/plan` puts the agent in a mode where every tool that writes, runs a command or reaches
the network is refused before it runs, and the model is told so: it reads, searches and
finishes with the plan as numbered steps. `/act` leaves it. `--plan` starts there, and a
mode key in the configuration makes that the default for a project.

This is not a stricter set of approvals. Someone who wants to know what would happen should
not have to decline fifteen prompts to find out, and an agent that must ask before every
step cannot describe the plan. Planning pairs with the model choice: plan on the model that
reasons best, switch to a smaller one, then act.

## One model per kind of work

A turn asks for very different things: a plan, a patch, a search through files, a small
JSON object to repair a malformed reply. One model for all of them is one compromise for
each. A project can say which does what:

```toml
model = "qwen3:8b"              # everything not named below

[roles]
plan = "qwen3.5:35b"            # reasoning is worth paying for here
edit = "qwen3-coder:30b"
search = "qwen3:1.7b"           # answered by a delegated session
repair = "qwen3:0.6b"           # reshaping a reply, nothing to reason about
```

Three of these are in force today. The repair round trip takes the repair model, since a
malformed reply has to be reshaped rather than thought about. A delegated question takes
the search model, which is the safest routing decision there is: a wrong choice costs one
sub-turn, not the conversation. And `/plan` moves to the plan model and `/act` gives it
back, saying so, because a model that changed without anyone announcing it makes the next
answer's character unexplainable. The status line shows the model answering now, which is
not always the one the session started with.

`review` names a second pair of eyes. When it is set, anything about to change something is
read first by that model, which is asked one question: does this do what was asked. An
objection stops the call before the file moves and is handed back to the model that
proposed it; agreement is one word and costs a few tokens on a card that was idle anyway.
The value is in the disagreement, which is why it is a different model: one checking its
own work agrees with itself. A reviewer that cannot be reached never blocks anything.

`escalate` is the model to raise to when the cheap path has been shown to be wrong. After a
write the language server is asked about the file, and, where the project has no language
server for it, the compiler is asked to compile and nothing else. A step whose change did
not check out raises the session to that model for the steps that follow, saying so. Two writes that
do check out bring it back down. A step that only read something moves nothing either way,
because a search says nothing about whether a model can produce a change that compiles.

What fixed a complaint here is remembered too. When a change does not check out, what the
check said is held; when a later change makes the same complaint go away, what that change
did is filed against it. The next time the same complaint appears it arrives with the note,
so the model does not work out from nothing what this project worked out last week. Line
and column numbers and the path in front of them are dropped from the signature: the same
mistake made twice is the same mistake at two places. `/why` lists what has been learned.

What each model has managed here is remembered between sessions: how many of its writes
checked out, per model, per project, kept beside the state on this machine and never in the
repository. When the cheap model has been failing most of its writes in a project, the next
turn starts on the stronger one and says so, instead of paying again for the bad step the
last session already paid for. Four writes before it is allowed to decide anything, and only
the last fifty are remembered, because a project changes. `/why` says what is in force and
what the evidence is.

That signal is the difference between this and a router in a chat window. A chat router can
only ask a model how confident it feels; a coding agent can compile. So the cheap model is
tried first and the expensive one is paid for where it has been shown to be needed, rather
than wherever a guess said the task looked hard.

`/model` lists them beside what the backend serves.

While a plan is being written, and while a tool runs during planning, the model that acting
will use is asked for in the background so the server reads its weights then rather than
when you type `/act`. A load is seconds of gigabytes onto a card, and those seconds pass
anyway while you plan. It is fire and forget: it never delays a turn, never fails one, and
never queues behind itself, and the server decides whether it can hold two models at once.
Only done against a backend on this machine or this network, since a hosted endpoint has
nothing to warm and would bill the attempt. `preload = false` turns it off.

## Choosing a model

The model flag is optional. What is asked for wins, then what a resumed session was last
using, then what the configuration remembers; failing all that the backend is asked, and
one that serves a single model leaves nothing to choose. With several and nothing
remembered the session opens with no model and says so: `/model` lists what the backend
serves, `/model <name>` switches now and writes the name into your own configuration for
next time.

A switch mid-session keeps the conversation and takes the new model's window from its own
table in the configuration when there is one, because a budget inherited from another
model either wastes the window or overruns it. What the last model taught about its
tokenizer is forgotten at the same time, and measured again from the first reply.

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

A server's documents are reached the same way: its resources become one tool that reads
them, `<server>__read_resource`, with what the server listed at startup named in the tool's
own description so the model can ask for one without a round trip to discover it. Reaching
them through a tool rather than a surface of their own is what keeps the approval flow, the
checkpoints and the transcript working on them unchanged.

`/mcp` says which servers are running and which prompts they offer; `/mcp <server>
<prompt>` fetches one and makes it this turn's prompt.

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

`auto` asks the backend once, at startup, for a call it is told to make. A server that
honours the `tools` field returns one; a server that takes the field and drops it returns
prose, which is the failure that costs the most and reports nothing - the model is never
told it has tools and every turn ends in a description of the call it could not make. The
session drops to the envelope either way, and also the first time a backend refuses the
field outright, finishing the turn either way. The status line says which protocol is in
force. Pin `--tool-protocol native` or `envelope` to skip the question.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

See [CONTRIBUTING.md](CONTRIBUTING.md).

## Licence

MIT OR Apache-2.0, at your option.
