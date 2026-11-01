# Models and roles

## What the engine measures

lokode reads what a loken server measured of each request.

| Reading | Where |
|---|---|
| how long the request waited, read the prompt and wrote the answer | the rail |
| how far the server has read a long prompt | the status line, while it reads |
| how full the key-value cache is | the rail |
| the power the machine's accelerators draw, while a turn runs | the rail |
| how much of a speculative draft the server kept | the rail |
| the energy each request drew | the rail, the receipt, `/cost`, the session file and `-p --output json` |

The power comes from the device listing loken serves to its own interface.
It is read every 2 seconds while a turn runs, and shown as read.  
The energy comes from the server, request by request.

Energy is counted by what a request was for: a step, a guessed read, a review, a repaired reply or the startup probe.  
A server that measures nothing shows nothing, never a zero.

| Mark | Meaning |
|---|---|
| `≈` | other requests shared the server |
| `~` | part of the energy was estimated |

## One model per kind of work

```toml
model = "qwen3:8b"              # everything not named below

[roles]
plan = "qwen3.5:35b"
edit = "qwen3-coder:30b"
search = "qwen3:1.7b"
repair = "qwen3:0.6b"
review = "qwen3:4b"
escalate = "qwen3.5:35b"
```

| Role | When it answers |
|---|---|
| `plan` | from `/plan` to `/act`, said on the screen both ways |
| `edit` | loaded in the background while a plan is written, ready for acting |
| `search` | a delegated question |
| `repair` | a malformed reply to reshape |
| `review` | before anything changes, to say whether it does what was asked |
| `escalate` | after a change fails its check |

The band shows the model answering now.

An objection from `review` stops the call and goes back to the model that proposed it.  
A reviewer that cannot be reached never blocks anything.

A step whose change fails its check raises the session to `escalate` for the steps that follow, and says so.
2 writes that pass bring it back down.  
A step that only reads moves nothing.

What each model managed is remembered per project, outside the repository: how many of its last 50 writes passed their check.

When the cheap model fails most of its writes in a project, the next turn starts on the stronger one and says so.
It decides only after 4 writes.  
`/why` says what is in force and on what evidence.

While a plan is written, the `edit` model, or else the default one, is asked for in the background, so the server loads it before `/act`.
It never delays or fails a turn.

The background request runs against a backend on this machine or this network only.
`preload = false` turns it off.

## Choosing a model

The model comes from, in order: the flag, the model a resumed session last used, the one the configuration remembers, and a backend that serves a single model.  
With several and none remembered, the session opens without one and says so.

`/model` lists what the backend serves, and `/model <name>` switches and remembers the name in your configuration.

<!-- site: figure name=choose-model-120 alt="The list of models, with what each one needs" caption="The list of models, with what each one needs" -->

A switch keeps the conversation.  
The new model's window comes from its own table in the configuration when there is one.
What the previous model taught about its tokenizer is measured again.

## Working it out while you are not waiting

`--discover`, or `discover = true` in the configuration, adds work done outside turns.
While nothing else runs, a read-only session answers one question about the project from a queue and writes down what it found.  
The queue and the findings outlive the session.

- Discovery runs only while nothing else does, and stops when you send a line.
- Discovery asks the server at a priority that yields.
- Every tool that writes, runs a command or reaches the network refuses.
- Discovery shows in the rail while it works, and `/jobs` stops it for the session.

What the discovery learned goes in front of the model next session, as a short paragraph.  
`/digest` shows it with its sources, and `/digest forget` clears it.  
What is worth keeping belongs in `.lokode.md`.

## What the project tells the agent

`.lokode.md` at the root of the project and `~/.config/lokode/lokode.md` for yourself are appended to the system message of every session.
Yours comes first, and each is named.

`/init` writes a first draft from the repository: the language, the test command, the layout.  
Keep it short: it holds the window for the whole session.
What does not fit is cut at a line boundary, and the session says so.

## Where the window stands

The window is read from the backend's model list (`max_model_len`), and `--context-tokens` caps it.
The status line shows how full it is: amber past 75%, red once the oldest steps start being elided.

`/context` says what holds the window, largest first: the instructions and the tools, what you said, what the model said, the tool results.  
`/compact` elides now.

<!-- site: figure name=context-120 alt="What fills the window, and how full it is" caption="What fills the window, and how full it is" -->

## What it cost

`/cost` says what the session spent: steps, tokens in and out, the rate, the time in the model and in tools, the energy by purpose, then the last steps one by one.

Every step is written to the transcript with what was known before it and what it cost.
