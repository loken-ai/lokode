# Using lokode

## Using it

A `/` at the start of a line opens the commands, and an `@` opens the files under the root.
Both lists narrow as you type.

<!-- site: figure name=complete-path-120 alt="A path is completed as you type after an @" caption="A path is completed as you type after an @" -->

| Key | In a completion list |
|---|---|
| `Tab` | take the item. A command that takes an argument gets a space after it |
| `Enter` | take a command and send it, or take a path |

A command's answer longer than a sentence opens on a page over the conversation.
`Esc`, `Enter` or `q` closes it.

`Alt-Up` and `Alt-Down` move between the steps of the session.  
What the step pointed at offers:

| Pointed at | Keys |
|---|---|
| a step | `Space` unfolds it, `Enter` opens it on a page, `Ctrl-C` stops it while it runs |
| a receipt | `u` puts the turn back, `d` shows what it changed |
| a question | `Enter` brings it back to the input |

| Key | |
|---|---|
| `Shift-Tab` | act, write without asking, or plan, in turn |
| `Enter` | send |
| `Shift-Enter` | newline |
| `Esc` | interrupt the turn, stop a command of yours, close a list, drop a selection, clear the line |
| `Esc` twice | bring back the last question |
| `Ctrl-F` | find something said earlier, then walk the matches with enter and up |
| `Ctrl-B` | show or hide the rail |
| `Ctrl-G` | write the prompt in `$EDITOR` and bring it back |
| `Ctrl-Y` | put the selection, or the block pointed at, on the clipboard |
| `y` / `a` / `n` | answer a change: once, always in the stated scope, no |
| `e` / `x` | refuse and say what to do instead, or refuse and correct the command yourself |
| `Enter` while it works | hand the line to the turn in flight |
| `Alt-Enter` while it works | ask it once the turn is over |
| `Up` / `Down` | walk what you cleared, then what you have already sent |
| `PageUp` / `PageDown`, `Home` / `End`, wheel | scroll |
| click | unfold a step, answer a question by its badge, take an item of a list, go to what runs from the rail |
| drag with the mouse | select here, and scroll when dragging past an edge |
| `Shift` + drag | select in the terminal itself, for its own copy key |
| `Ctrl-T` | hand the mouse to the terminal, or take it back |
| `Ctrl-C` | stop the step pointed at, stop the turn or a command of yours, clear the line, or leave when pressed twice on an empty line |
| `Ctrl-D`, `Ctrl-Q` | leave |

lokode asks before something is lost.
Leaving asks for the key a second time.  
The others open a list with `no` selected.

| Action | Asks when |
|---|---|
| `Ctrl-D`, `Ctrl-Q`, `Ctrl-C` on an empty line | a turn runs, a question waits or a line is typed |
| `/clear` | always |
| `u` on a receipt | always |
| `Enter` on a question | turns came after it |

| On the line | |
|---|---|
| `!command` | run the command here, beside the screen, under a step of its own. Its output joins the conversation |
| `#rule` | append a rule to `.lokode.md` and tell the model at once |
| `@path` | send that file, or that directory's listing, with the question |

| Command | |
|---|---|
| `/plan`, `/act` | plan without changing anything, then carry it out |
| `/model` | choose from what the backend serves, with the one answering marked |
| `/clear`, `/resume` | start again, or come back to a session recorded earlier |
| `/retry`, `/retry <model>` | ask the last question again, of the same model or another |
| `/copy`, `/copy last` | the conversation, or the last answer, to the clipboard and to a file |
| `/undo`, `/diff` | put the project back to before a turn, or see everything changed |
| `/context`, `/compact`, `/cost` | where the window stands, elide now, what it spent |
| `/jobs` | what is running now, to point at its step or stop work done outside a turn |
| `/lsp` | the language servers: where each stands, the files it reports, why one is down |
| `/approve`, `/approve forget` | what is in force here and in the configuration, or take it back |
| `/net`, `/net save` | what commands reached over the network, or share the hosts you agreed to with the project |
| `/why`, `/digest`, `/tools`, `/agents`, `/skills`, `/plugins`, `/mcp` | what is in force, what is known, what can be called |

A command that takes a name offers the names.
`/model`, `/undo`, `/resume` and `/mcp` open a list that narrows as you type.  
`/help` lists every command, with those the project ships.

The window title says whether the session works or waits for you.
Ghostty, WezTerm, Windows Terminal and iTerm2 from 3.6 also show it as progress in the tab.

While the terminal is not in front, a waiting question and the end of a turn arrive as a desktop notification, or as a bell where the terminal has none.

`Ctrl-Y` reaches the terminal's clipboard through ssh and tmux.

Answers are rendered as markdown: fenced code coloured by language, headings, lists, quotes, tables, links and emphasis.  
A diff is numbered, with the changed words marked.
There are no borders.

A line sent while the agent works joins the turn at the next step, and the screen says when.
A slash command typed then runs at once.

The status line says what the attached files cost before the line is sent.
They are bounded like any other content.  
A mention that is not a path here is left alone, and nothing outside the project can be named.

A shell command and a test suite show the end of their output under their step while they run.
The tool's result replaces it when they finish.  
The model reads the whole output.

The status line says what the turn is doing and for how long: fitting the history, waiting for the model, reading the prompt, thinking, answering, repairing a reply or running a tool.

While loken reads a long prompt, it shows how far it has got.

While the model writes its answer, a small model guesses the next read and the read is made ahead.
The step that asks for it is answered without a round trip, and its row says `guessed`.

A guess never delays the turn, and a guess that would change something is dropped.

The guessing model is the project's search model, or the one answering.
It runs against a backend on this machine or this network, never a hosted one.  
`speculate = false` turns it off.

A turn ends on a receipt: its time, its steps, what it read and wrote, the files it changed and its energy.
A quick turn that changed nothing gets none.

The loop declines 3 things:

- a read already made this turn, with nothing written since, is answered with its result, and the model is told it repeats itself
- a whole-file write over a file changed on disk since the model read it is refused
- a path that does not exist is answered with where the file is

`/retry` asks the last question again.  
What the agent did after it leaves the conversation.

`/retry <model>` asks another model.
The transcript keeps both attempts, and `/undo` puts back the files of the first.

A turn that gets nowhere is handed back early.
3 steps in a row where every call failed or repeated itself end it, and so do 6 failed calls in a turn that changed nothing.  
The screen names the tool that kept failing.

At an approval, a click on a badge answers like its key.
`Enter` does nothing there.

## What runs

The rail opens with what is running: each step that has not ended with how long it has run, the call a delegated session is on, the work done outside a turn, and a language server reading or building the project.

<!-- site: figure name=busy-120 alt="A turn in progress: what runs is listed in the rail" caption="A turn in progress: what runs is listed in the rail" -->

| To | Do |
|---|---|
| go to a step that runs | choose it in `/jobs`, or click it in the rail |
| stop one call while the turn goes on | `Ctrl-C` on its step |
| stop the whole turn | `Esc` |
| stop a command of yours | `Ctrl-C` on its step, or `Ctrl-C` or `Esc` on an empty line for the latest |
| stop work done outside a turn | choose it in `/jobs` |
| see where the language servers stand | `/lsp`, or choose a server in `/jobs` |

A stopped call ends in an error that tells the model the person stopped it, and the turn goes on.

Work outside a turn is the model held ready on the backend and the project being looked into.
Looking into the project, once stopped, stays stopped for the session.

Commands of your own run side by side, each under its step, with their output as it comes.
