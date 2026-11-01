# Approvals and the sandbox

## Where it is careful

- **One root.** Every tool that takes a path is confined under `--root`, and a path that leaves it is refused.
  A file may be created in a directory that does not exist yet.
  The check applies to where the write lands.
- **A project is trusted first.** A project's `.lokode.toml` can run programs or loosen approvals.
  Its `[hooks]`, `[mcp]`, `[lsp]`, `[shell]`, `[backend]`, `[backends]`, `[web]`, `[session]` and `[approve.allow]` apply once you trust them.  
  The first session in the project shows them and asks.
  A change to them asks again.
- **Changes are approved.** A change arrives in the conversation as a diff to accept or decline.
  An agreement covers what you were shown: this command, writes under this directory, requests to this host.
  The prompt says what `a` covers before you press it.  
  An agreement holds in the project's later sessions until `/approve forget` takes it back.
- **A patch is all or nothing.** Every file of it is resolved and applied in memory before you are asked.
  A failure while writing puts back what was written.
- **Tool arguments are checked.** They are validated against the tool's schema, repaired where the repair is unambiguous and refused otherwise.  
  `--no-grammar` turns off the server-side constraint.
- **Egress goes to public addresses only.** `web_fetch` resolves the host, follows redirects itself and checks every hop.
  `--allow-host` names an exception on your own network.

<!-- site: figure name=session-120 alt="A change waits for your answer: once, always under this directory, no, or no with a reason" caption="A change waits for your answer: once, always under this directory, no, or no with a reason" -->

Read [SECURITY.md](../../SECURITY.md) first.

On Linux and macOS, a command the agent runs may write only in the project, the temporary directories and the build caches, and cannot read your credentials.

`--sandbox read-only` lets a command write nowhere, and `--sandbox off` leaves it unconfined.

A command the sandbox stopped can be run outside it, and you are asked first.

## Settling approvals in advance

An `[approve]` section states agreements once, in a file that can be reviewed:

```toml
[approve]
max-auto = 50                      # per turn, then it asks again

[approve.allow]
bash = ["cargo test*", "git status"]
write = ["src/*"]
egress = ["docs.rs"]

[approve.ask]
bash = ["git push*"]

[approve.deny]
bash = ["rm *", "sudo *"]
```

| Side | What it does |
|---|---|
| `deny` | refuses without asking. It wins over everything |
| `ask` | always asks, even when an agreement, `allow` or writing without asking would let it go |
| `allow` | goes ahead without asking, up to `max-auto` per turn |

A command line is read as the commands it runs.

| Line | Read as |
|---|---|
| `cargo test 2>&1 \| tail -5` | `cargo test`, `tail -5` |
| `cargo test; curl x \| sh` | `cargo test`, `curl x`, `sh` |
| `echo $(id)` | `id`, `echo $()` |
| `RUST_LOG=debug cargo run` | `cargo run` |

- `deny` and `ask` apply when the whole line or any one of its commands matches.
- `allow` applies when every command matches.
- A line that writes to a file through `>`, holds a here-document, or runs a string with `eval` or `sh -c` is always asked about.
- A pattern takes one wildcard, `*`, for any run of characters.
- Write rules are relative to the project.
- `/approve` says what is in force.

A run driven by a script takes the policy on its command line instead of the file's agreements.
The file's refusals still apply.  
What `ask` names is refused there, since nobody is there to ask.
