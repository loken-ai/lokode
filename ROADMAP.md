# Roadmap

The next work. [`CHANGELOG.md`](CHANGELOG.md) lists what shipped.

A row stays until its `Done when` is true.

| State | Meaning |
|---|---|
| Started | Work is under way |
| Done | Shipped |
| Partial | Part shipped, the rest is open |
| Planned | Not started |

| Feature | Category | Done when | State |
|---|---|---|---|
| Sound notifications | Operations | A sound tells that an action has finished, and says whether it succeeded or failed. The sounds are declared like hooks, for the end of a turn, of a command, of a sub-agent and of a question that waits for an answer. | Done |
| Sandbox on Windows | Security | A command on Windows writes only in the project, the temporary directories and the build caches, cannot read the paths marked hidden, and reaches the network only through the proxy. | Partial |
| Sandbox without a network on Windows | Security | With the network closed, a command on Windows opens no connection and reads no file of the person outside the project. | Planned |
| Commands in WSL2 | Security | On a Windows machine with WSL2, a command can run in a Linux distribution under the Linux sandbox. | Planned |
| Sandbox checked on every system | Tests | The sandbox tests pass on macOS, on Windows and on Ubuntu with AppArmor, and not only on Debian. | Planned |
