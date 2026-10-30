# Hooks, commands, skills, plugins and tools

Everything that adds to lokode without changing it.

## Hooks

```toml
[hooks]
pre-tool = "./scripts/guard.sh"
post-tool = "cargo fmt"
turn-end = ["cargo fmt --check", "cargo test --quiet"]

[[hooks.pre-tool]]
match = "read_file|grep"           # tools by name, * for any run of characters
command = "./scripts/no-secrets.sh"
timeout = 10
```

A hook is a command, or a table with `command`, `match` and `timeout`, alone or in a list. The
user's hooks and the project's both run, the user's first.

| Event | When | What blocking does |
|---|---|---|
| `session-start` | before the first question of a session | nothing. What it says goes with the question |
| `prompt` | when a question is asked, before the model sees it | refuses the question. What it says goes with the question |
| `pre-tool` | before a tool runs | refuses the call. Its JSON may give other arguments |
| `post-tool` | after a tool ran | nothing. What it says goes with the result |
| `turn-end` | when the model has answered, or the turn ended otherwise | hands the reason to the model and the turn goes on, twice at most |
| `agent-end` | when an agent has answered | nothing. What it says goes with the answer |
| `compact` | when the oldest steps have left the window | nothing. What it says goes to you |
| `session-end` | when the session ends | nothing |
| `notification` | when the session waits for you, as for an approval | nothing. It does not hold the session up |

A tool hook without `match` runs around the tools that change something.

A hook reads one JSON object on its standard input: the event, the session, the project, and
what the event is about (the tool and its arguments, the result, the question, the answer). It
also finds `LOKODE_HOOK`, `LOKODE_TOOL` and `LOKODE_ROOT` in its environment.

| Exit | Meaning |
|---|---|
| 0 | go on. What it printed is handed on, or read as a decision when it is a JSON object |
| 2 | block. Its error output, or else its output, says why |
| other | it failed. A failed `pre-tool` hook refuses the call |

A decision is `{"decision": "block", "reason": "..."}`, with `context` for what to hand the
model and `args` for the arguments a `pre-tool` hook wants the call made with. A hook that
outlives its timeout, 30 seconds by default, is stopped with everything it started. Its output
is bounded.

## Commands written as files

A Markdown file is a command, named by its file: `review.md` makes `/review`, and
`git/commit.md` makes `/git:commit`. Its text becomes the prompt.

```text
.lokode/commands/git/commit.md   (or ~/.config/lokode/commands/git/commit.md)

---
description: commit what is staged, in the project's style
argument-hint: [scope]
agent: scribe
---
Write a commit message for what is staged, scoped to $1.
```

| Setting | What it does |
|---|---|
| `description` | what `/help` and the list of commands say of it. The first line of the prompt without it |
| `argument-hint` | what it takes, shown beside it |
| `agent` | the agent the prompt is handed to |

- What is typed after the name replaces `$ARGUMENTS`, and its words replace `$1` to `$9`. A
  word in double quotes keeps its spaces. A command that names none of them has it appended.
- The project's command takes the place of the user's of the same name.
- A built-in keeps its name: a file named after one is not read.

## Skills

A skill is know-how the model loads when a task calls for it: a directory with a `SKILL.md`,
named by the directory.

```text
.lokode/skills/release/SKILL.md   (or ~/.config/lokode/skills/release/SKILL.md)
.lokode/skills/release/scripts/notes.sh

---
description: cut a release of this project
---
1. Bump the version in Cargo.toml.
2. Write the notes with scripts/notes.sh.
```

- The model sees each skill's name and description, and nothing more until it loads one with
  the `skill` tool. The window holds what is used.
- Loading a skill gives its text and the names of the files it keeps. The tool loads one of
  them by name, and nothing outside the skill's directory.
- The project's skill takes the place of the user's of the same name.
- A skill without a description is refused, since the description is what the model chooses
  it by.

`/skills` lists them.

## Plugins

A plugin brings commands, agents, skills, hooks and tool servers together. It is a directory
in `~/.config/lokode/plugins/`, named by the directory:

```text
~/.config/lokode/plugins/release/
  plugin.toml
  commands/ship.md
  agents/checker.md
  skills/notes/SKILL.md
```

```toml
# plugin.toml
description = "the team's release routine"
version = "1.2.0"

[hooks]
turn-end = "cargo fmt --check"

[mcp.servers.tracker]
command = "tracker-mcp"
```

- A plugin is installed by putting its directory there, with `git clone` for instance.
- Its commands, agents and skills come after the user's and before the project's.
- Its hooks run after the user's and before the project's.
- A tool server keeps the name the plugin gives it, and the first to use a name keeps it.
- `[plugins] disabled = ["release"]` in the user's configuration turns one off.
- Plugins live in the user's configuration only: a project cannot bring one.

`/plugins` lists them, what each brings, and which are off.

A plugin kept in git is installed and kept up to date from the command line:

| Command | What it does |
|---|---|
| `lokode plugin install <git url>` | Clones it under the end of the url, or under `--name` |
| `lokode plugin update [name...]` | Brings the named plugins, or all of them, up to their source |
| `lokode plugin remove <name>` | Takes it out |
| `lokode plugin list` | Lists what is installed and what each brings |

A plugin whose hooks or servers run programs shows them and asks before it is installed. An
update that changes what runs asks again. `--yes` takes it without asking, which a script needs.

A marketplace is a git repository that lists plugins in a `marketplace.toml`:

```toml
description = "the team's plugins"

[[plugins]]
name = "release"
description = "the release routine"
source = "https://example.com/team/lokode-release.git"   # a git url

[[plugins]]
name = "fmt"
source = "plugins/fmt"                                    # or a directory of the marketplace
```

| Command | What it does |
|---|---|
| `lokode plugin marketplace add <git url>` | Adds it under the end of the url, or under `--name` |
| `lokode plugin search [term]` | Lists what the marketplaces offer, and what is installed |
| `lokode plugin install release` | Installs a plugin one marketplace alone lists |
| `lokode plugin install release@team` | Installs it from that marketplace |
| `lokode plugin marketplace update [name...]` | Brings marketplaces up to their source |
| `lokode plugin marketplace remove <name>` | Takes one out. Its plugins stay installed |

`lokode plugin update` brings a plugin copied from a marketplace up to it, after updating the
marketplace.

### Plugins from other tools

A plugin written for Claude Code installs and loads as it is, from git or from a marketplace
that lists it in `.claude-plugin/marketplace.json`. lokode reads what it has a place for.

| In the plugin | In lokode |
|---|---|
| `skills/`, `commands/`, `agents/` | Loaded. Settings lokode does not use are ignored |
| `hooks/hooks.json`, the manifest's `hooks` | `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `Stop`, `SessionStart`, `SessionEnd`, `Notification`, `SubagentStop` and `PreCompact` |
| `.mcp.json`, the manifest's `mcpServers` | Tool servers, over a command or HTTP |
| `.lsp.json`, the manifest's `lspServers` | Language servers, by their extensions |
| `userConfig` | Values for `${user_config.KEY}` in servers: `[plugins.options.<plugin>]`, or `CLAUDE_PLUGIN_OPTION_<KEY>`, or the default |
| `${CLAUDE_PLUGIN_ROOT}` | The plugin's directory, in commands, agents, skills, hooks and servers |
| A marketplace source | A path, `github`, `url`, `git-subdir`, `archive` or `npm`, each updated from where it came |

- A hook's matcher names tools as Claude Code does, `Write`, `Edit`, `Bash`, `Read`, `Grep`,
  `Glob`, `WebFetch`, `Task` and `mcp__server__tool`, and lokode matches its own tools by them.
- A hook is given `hook_event_name`, `tool_name`, `tool_input`, `tool_response` and `cwd` beside
  lokode's own fields, and finds the project in `CLAUDE_PROJECT_DIR`. Exit code 2 blocks, and so
  does `permissionDecision` `deny`.
- An agent's `tools` and a hook's `matcher` are translated. Its `model`, when it is `sonnet`,
  `opus`, `haiku` or `inherit`, is ignored, so the agent runs on the models you configured.
- An option a plugin requires and nobody gave is said when the plugin is listed, with where to
  set it. An archive is unpacked as ordinary files, without links or special permission bits.
- Output styles, themes and monitors have no counterpart in lokode and are not read. What else
  a plugin holds that lokode cannot use is said when the plugin is listed: a hook event it does
  not have, a hook that is not a command, a server with neither command nor url.

## Tools from elsewhere

A Model Context Protocol server you configure adds its tools, named `<server>__<tool>`.

```toml
[mcp.servers.github]
command = "mcp-server-github"
args = []
env = { GITHUB_TOKEN = "..." }     # prefer naming a variable your shell already has
timeout = 30                       # seconds for one answer, the handshake included

[mcp.servers.docs]
url = "https://mcp.example.com/mcp"   # spoken to over HTTP, in place of a command
token-env = "DOCS_TOKEN"              # sent as Authorization: Bearer
headers = { "X-Team" = "lokode" }

[mcp.servers.tracker]
url = "https://tracker.example.com/mcp"   # signed in to with /mcp login tracker
client-id = "..."                         # only where the server does not register clients
scopes = ["read"]                         # only where the server does not say
```

- Every call is approved, and treated as able to change the project, unless the server marks
  the tool `readOnlyHint`. Such a tool runs without a question, while planning too.
- The servers start side by side. One that stops fails what waits on it at once, and is
  stopped for good at the end of the session.
- Stopping a call tells the server it is withdrawn.
- The server's own requests are answered: `ping`, and `roots/list` with the project.
- A server may ask the model a question. You approve it like a tool call, and `a` covers that
  server for the session. The session's model answers, within its answer budget, whatever model
  the server names.
- A server may ask you for values. Each field is asked in turn on the input line, what must be
  given first. Enter answers a field and esc declines the form. A run with `-p` declines.
- Text, embedded documents and structured content reach the model. An image or a sound is
  named, not shown.
- The schema a server advertises is checked before it shapes the grammar. A tool that fails the
  check is refused by name.
- A server that does not start takes only itself down.
- Its resources become one tool, `<server>__read_resource`, whose description lists them.

- A server that says its tools changed has them taken again at the next turn.

`/mcp` shows each server, whether it runs, its tools, its protocol and how it is reached, with
what a stopped one said last. It lists the prompts and what each takes.
`/mcp <server> <prompt> name=value` makes one the turn's prompt. A value in double quotes takes
spaces, and an argument the prompt needs is asked for before the server is.
`/mcp restart <server>` starts a server again as it was configured, and takes its tools again.

`/mcp login <server>` signs in to a server over HTTP that wants a signed-in user:

- The browser opens at the server's sign-in page, and the address is written in the thread.
- The browser comes back to a port on the loopback address. The code it brings is bound to a
  secret that never leaves lokode.
- The token is kept in `mcp-tokens.json` in the data directory, readable by you alone, and
  renewed when it runs out.
- The server starts once you are signed in, a server that would not start included.
- `/mcp logout <server>` forgets the token.
