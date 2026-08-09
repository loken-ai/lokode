# Security

verve gives a language model the ability to read your files, change them, reach the network
and run commands. This document says what is actually enforced and what is not, because a
guarantee you believe in and do not have is worse than one you know you lack.

## What is enforced

**Path confinement.** Every tool that takes a path resolves it through one function, which
canonicalises the target and refuses anything that does not land under `--root`. It refuses a
symlink that leaves the root, and a symlink whose target does not exist yet - the two-step
version, where creating the link is itself an allowed operation. A file may be created under
directories that do not exist yet; the check applies to where the write would land, not to how
the path was written. Resolution and use are separate steps, so a link swapped between the two
would defeat it: the guarantee is against a confused model, not against another process on the
machine racing it.

**Approval before anything changes.** Writing a file, applying a patch, running a command,
running the test suite and fetching a URL each require a decision. `Enter` is not one of the
accepting keys.

**Scoped agreements.** `[A]lways` is remembered against what the prompt showed - this exact
command, writes by this tool under this directory, requests to this host - so agreeing to one
`ls` does not hand over the shell. A command is matched in full rather than by the program at
the front of it, because `ls` and `ls; rm -rf ~` share a first word.

**Egress.** `web_fetch` accepts only `http(s)`, resolves the host rather than trusting the
name, and refuses loopback, link-local (including the cloud metadata address), RFC1918, CGNAT
and unique-local addresses. It follows redirects itself so the same check runs on every hop,
and asks again when a redirect leaves the host you approved. `--allow-host` opens a named
exception.

**Bounded reads.** A file over 8 MiB is refused rather than loaded, a response body is
streamed against a ceiling, and a file that is not text is refused rather than delivered as a
screenful of replacement characters.

**Atomic writes.** Files are replaced by writing beside them and renaming, so an interruption
leaves the old contents or the new ones. A multi-file patch is applied wholly or not at all.

## What is not enforced

**`bash` is not confined by the root.** An approved command runs `/bin/sh -lc <string>` with
your privileges and your environment. Only its working directory is checked. `cd ~ && cat
.ssh/id_rsa` is one tool call, and `-l` sources your login profile. The path sandbox describes
the tools that take paths; it says nothing about a shell. Approving a command is approving
everything that command can do.

**`run_tests` runs the project's own code.** `cargo test` builds `build.rs`, `npm test` runs
lifecycle scripts, `pytest` imports `conftest.py`. Approving it on a repository you have not
read is approving whatever that repository wants to run.

**Nothing is redacted.** `.env`, `~/.aws/credentials` inside the root, a `.git/config` with a
token in it - all readable by the model if they are under the root, and all recorded in the
session transcript. The transcript is written with your umask; nothing in it is encrypted.

**The change history holds copies of your files.** `/undo` and `/diff` work because every
change is committed to a git repository under `$XDG_DATA_HOME/verve/checkpoints/<session>`,
whose work tree is the project. It therefore contains full copies of everything under the root
that is not ignored - including anything sensitive - and it outlives the session. It respects
the project's `.gitignore`, so what git would not track it does not keep, but that is the only
filter. Delete the directory when you are done with the session.

**The model is not trusted, but it is believed.** Tool arguments are validated and repaired,
and a call to a tool that does not exist is refused. What is not checked is intent: a
well-formed call to delete the right file is indistinguishable from a well-formed call to
delete the wrong one. Approval is the only thing between the two, which is why the prompts try
to say what they are actually asking.

**Prompt injection is not addressed.** Content read from a file or fetched from the web
reaches the model as ordinary text. A repository that contains instructions aimed at an agent
will have them read. The approval prompts are the mitigation, and they only work if you read
them.

## Running it more safely

- Point `--root` at one project, not at your home directory.
- Decline `[A]lways` for `bash` unless you know what the session is going to do.
- Prefer a container or a VM for a repository you have not read.
- Keep an API key in the environment rather than in a config file. If you do put one in a
  file, verve refuses to use it unless the file is private to you.
- Read `$XDG_STATE_HOME/verve/verve.log` and the session transcript afterwards. Every approval
  decision and every tool call is in them.

## Reporting a vulnerability

Open an issue at https://github.com/loken-ai/verve/issues. If it is something that
should not be public first, say so in an issue with no detail and a maintainer will arrange a
private channel.

Things worth reporting: a way out of `--root` through a path-taking tool, a way to reach a
private address through `web_fetch`, a way to get a tool to run without the approval it is
supposed to require, or a way to make a credential appear in a log or on screen.
