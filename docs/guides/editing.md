# Reading, changing and undoing

## The tools

A tool that can change something, reach the network or run code is asked about first.

| Tool | What it does | Asked |
|---|---|---|
| `read_file`, `list_dir`, `glob`, `grep` | read, list and search under the root | no |
| `write_file`, `edit_file`, `edit_lines`, `apply_patch` | change files, as a diff to accept | yes |
| `bash`, `run_tests` | run a command, or the project's tests, in the sandbox | yes, per command |
| `web_fetch` | fetch a page from a public address | yes, per host |
| `lsp_definition`, `lsp_references`, `lsp_implementations`, `lsp_hover`, `lsp_calls`, `lsp_symbols`, `lsp_diagnostics` | ask a language server | no |
| `update_plan`, `submit_plan` | keep the task list, and put a plan to you | a plan, yes |
| `delegate` | hand a task to an agent | no, what the agent does is asked |
| `take_work` | bring in what an agent did in its own copy | no, its writes were asked in the copy |
| `skill` | load a skill and the files it keeps | no |
| `<server>__<tool>` | what a tool server offers | yes, unless it says it only reads |

## Changing a file

| Tool | |
|---|---|
| `write_file` | replace a file |
| `edit_file` | replace one unique substring |
| `edit_lines` | apply several such edits to one file in order, all or none |
| `apply_patch` | apply a unified diff across several files |

After a write, the language servers are told the files changed and were saved.
Once they have read the new text and have no work left, lokode compares what they report with what they reported before.

The tool result lists the errors the write introduced, in any file, with their code and source line.

Without a language server for the file, the compiler is asked to compile, once per step and with a bounded wait.

| A write's step says | When |
|---|---|
| `checked` | the servers or the compiler report no new error |
| `check failed` | they report errors the write introduced |
| `not checked` | none could tell, and the result says why |

The rail's CHANGES marks a file the servers report errors in with `✕` and their count.  
Only a write that was checked counts when the session decides to escalate.

When a change fails its check and a later change clears the same complaint, the fix is filed against the complaint.
The next occurrence arrives with the note.

Line and column numbers and paths are left out of the comparison.  
`/why` lists what has been learned.

## Asking the language server

| Tool | What it answers |
|---|---|
| `lsp_definition` | where a symbol is defined |
| `lsp_references` | every place a symbol is used |
| `lsp_implementations` | the implementations of a trait, an interface or an abstract method |
| `lsp_calls` | the functions that call a function, or that it calls |
| `lsp_hover` | the type and the documentation of a symbol |
| `lsp_symbols` | the outline of a file, or the symbols matching a name across the project |
| `lsp_diagnostics` | what the server reports about a file |

<!-- site: figure name=lsp-page-120 alt="The language servers, what each one is doing" caption="The language servers, what each one is doing" -->

| Language | Server | Extensions |
|---|---|---|
| rust | `rust-analyzer` | rs |
| python | `pyright-langserver --stdio` | py, pyi |
| typescript | `typescript-language-server --stdio` | ts, tsx, mts, cts, js, jsx, mjs, cjs |
| go | `gopls` | go |
| c | `clangd` | c, h, cpp, cc, cxx, hpp, hh, hxx |
| zig | `zls` | zig |
| bash | `bash-language-server start` | sh, bash |
| lua | `lua-language-server` | lua |
| ruby | `ruby-lsp` | rb |

At startup the session reads which of these languages the project's files are written in, and starts their installed servers in the background.
The tools are offered only when one of them can answer for the project.

A server that stops is started again once.
A second stop, or a server that cannot start, leaves it down for the session with what it said.

The `[lsp]` section changes a built-in server, turns one off or adds one:

```toml
[lsp]
enabled = true                       # false turns every language server off

[lsp.servers.rust]
settings = { rust-analyzer = { check = { command = "clippy" } } }

[lsp.servers.ruby]
enabled = false

[lsp.servers.elixir]                 # a new language needs a command and its extensions
command = "elixir-ls"
extensions = ["ex", "exs"]
```

A server also takes `args`, `init` for its initialization options and `env`.  
`settings` is what the server is answered when it asks for its configuration.

`LOKODE_LSP_<LANGUAGE>`, as in `LOKODE_LSP_RUST`, replaces a server's command and arguments, which helps when the program on your `PATH` is a `rustup` shim for a component that is not installed.

A symbol is named by its file, its line and its name as written there.
The column is optional.  
A name not found on the line is looked for on the lines around it, and the answer says where it was found.

Each place found comes back as `path:line:column` with its source line.

## Taking something back

Every change the agent makes is committed to a git repository kept beside the session, with the project as its work tree.
Your own repository is not touched.

```
/diff    everything the agent has changed this session
/undo    put back the most recent turn that changed something
/help
```

`/undo` again goes one turn further back.
An undo is itself a commit carrying the older state, so an edit you made by hand in between stays in the history.

`u` on a receipt puts that turn back, after asking.

See [SECURITY.md](../../SECURITY.md): the history holds copies of your files and outlives the session.

## Sessions

Each session is written to `$XDG_DATA_HOME/lokode/sessions/<id>.jsonl` as it runs.

```sh
lokode --continue              # the most recent session for this root
lokode --resume <id>           # a particular one
lokode --no-transcript         # record nothing
```

A resumed session keeps its id, appends to its file and remembers its model, so `--model` becomes optional.

Logs go to `$XDG_STATE_HOME/lokode/lokode.log`, never to the terminal.  
`RUST_LOG=lokode=debug` raises the level, and `--log-file` moves the file.
