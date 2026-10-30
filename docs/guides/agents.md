# Plans and agents

Plan before acting, hand work to an agent, and keep work of several steps in order.

## Planning before acting

`/plan` refuses every tool that writes, runs a command or reaches the network, before it runs,
and tells the model so. The model reads, searches and puts its plan to you with `submit_plan`:
a summary and numbered steps, asked like a change.

| Key | What happens |
|---|---|
| `y` | planning ends, the steps become the task list, and the turn carries on acting |
| `e` | the plan is refused with what to change, and the model plans again |
| `n` | the plan is refused, and the session keeps planning |

`/act` leaves the mode without a plan. A rule `tools = ["submit_plan"]` under
`[approve.allow]` acts on every plan without asking. A run without a terminal acts on it only
with `--approve all`.

| Mode | Shift-Tab | Flag | `mode =` |
|---|---|---|---|
| act, every change asked | from plan | none | `"act"` |
| write without asking, commands still asked | from act | `--accept-edits` | `"edits"` |
| plan, nothing changes | from writing | `--plan` | `"plan"` |

Nothing has to be declined while planning. A project can plan on its strongest model and act
on a smaller one, with [roles](models.md#one-model-per-kind-of-work).

## Handing work to an agent

`delegate` gives one self-contained task to an agent, in a session of its own with its own
window, and returns only its answer. The work does not fill the conversation's window.

Its steps stay under its own step, folded into a count once it ends. `Space` unfolds them, and
`Enter` opens a page with each step, its evidence and the answer. `Ctrl-C` on its step stops it
alone.

`explore` is built in. It searches the project to answer one question and changes nothing.
Other agents are Markdown files, named by their file:

```text
.lokode/agents/reviewer.md   (or ~/.config/lokode/agents/reviewer.md)

---
description: reviews the changes for bugs
tools: read_file, search, lsp_at, bash
model: qwen3:4b
max-steps: 12
---
Read the changes since the last commit and say what is wrong with them.
```

| Setting | Without it |
|---|---|
| `description` | the first line of the prompt. The model reads it to choose |
| `tools` | the tools that change nothing |
| `model` | the `search` role for an agent that changes nothing, the `edit` role otherwise, else the session's model |
| `max-steps` | 8 for an agent that changes nothing, the session's limit otherwise |
| `backend` | the session's backend. Another one is named in `[backends]`, and then `model` is needed |

An agent can run on another machine or another API:

```toml
[backends.amd]
url = "http://amd.local:11435"

[backends.claude]
url = "https://api.anthropic.com"
api-key-env = "ANTHROPIC_API_KEY"
```

- The project's agent takes the place of the user's of the same name.
- An agent never has the task list or `submit_plan`. It has `delegate` when it names it and
  `[limits] depth` leaves a level under it. The depth is 1 by default, so no agent hands work
  on. Each level has its own `[limits] delegates` places.
- What an agent changes is asked about like the session's own changes, checked by the language
  servers and run through the hooks. It is put back with the caller's turn by `/undo`.
- While the session plans, an agent changes nothing.
- At most `[limits] delegates` agents run at once, 2 by default. The others wait.
- An agent out of steps says it did not finish, with what it said last.
- With `background`, an agent works while the turn goes on. Its answer is handed to the session
  at its next step, in this turn or the next. Stopping the turn does not stop it. It is listed in
  the rail and in `/jobs`, where enter stops it alone.
- In a project in git, an agent that changes files works in its own copy, a worktree in lokode's
  state directory, made from the project as it stands: files not committed and files git does
  not track included, ignored files left out. Its tools and its shell write there alone, and its
  changes are approved like any other. Nothing reaches the project until the session brings the
  work in.
- Its answer comes with the patch and a number, and the session calls `take_work` with it. The
  work comes in as one change `/undo` puts back. A turn does not end while work waits.

| Since the agent started, the project | `take_work` |
|---|---|
| changed nothing | brings the work in |
| changed files | writes nothing, and shows what changed and both sides of each file both touched |
| changed files, and the session wrote those both touched | brings in the rest, given `merged` with a proof for each |

- The review gives both objectives: the task the agent was given with what it said it did, and
  the last questions asked since. It repeats neither the agent's patch nor an untouched file.
- A proof names the file, the agent's goal, the project's goal and how the file serves both.
  Each line either side added must be in the file, and each line either side removed must be
  gone. A line left out on purpose goes in `dropped`, with why.
- A refusal lists only the lines the proof does not account for, and writes nothing.
- Once in, the files are checked like any write. An error comes from putting both together,
  and the session fixes it before the turn ends.
- When the project changed meanwhile, a model reads both sides together once, and says what
  would not work together: a name or signature one side changed and the other uses, the same
  thing done twice, behaviour that contradicts. It is the `review` role's model, or the
  session's. It is given the objectives and both sides' changes without their context, cut
  short, and answers `OK` or 5 lines at most. What it finds is the session's to fix before
  the turn ends.
- An agent stopped by the user is not brought in. Its copy stays, its path said.
- A proof refused twice in a turn counts as a check that failed: with an `escalate` model named
  in `[roles]`, the session carries on with it to do the reconciling.
- A project outside git is copied through a private repository kept beside the copy, which holds
  where the agent started. The project is never made a repository. Files `.gitignore` names are
  left out, and a project over 256 MB is not copied.
- When there is no copy, because the project is too large or its repository does not track it,
  the agent writes in the project. A file it writes is its own until it answers: the session
  and the other agents are refused it, before anyone is asked.
- An answer that arrives while no turn runs starts a turn, so the work is taken up at once.
- The task is taken as written: no file is attached for it and the project is not searched
  for it.

A line that opens on `@` and an agent's name hands the rest of the line to that agent. `@`
offers the agents before the files.

`/agents` lists the agents, what each may use and where it was read from.

## Work of several steps

`update_plan` is a tool the model calls to keep a task list: done, in progress, left. The rail
shows it under PLAN with how far it has got. It writes nothing and needs no approval.

- A resumed session comes back with its task list, and planning if it was planning.
- When the oldest steps leave the window, the note left in their place carries the list as it
  stood, so the model keeps the goal.
- `/clear` empties the list.
