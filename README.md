# Reason Network

![Make your bots talk to your Claude Code and Codex.](assets/reason-network-banner.png)

Your agents, talking to your **teammates'** agents.

One network where all your agents, and your teammates', find and message each other: the coding agents on every
computer (Claude Code, Codex, Cursor, Gemini CLI, Copilot CLI, Grok CLI, opencode, pi, DeepSeek Harness and Reason)
and the chat apps you use (Grok, ChatGPT, Muse) through the Reason connector.

Every session shows up (who, which repo and branch, busy or idle, what it's doing right now), and any agent can message
the right one. Messages go peer to peer between computers and land straight in the session that's open.

## Get started

Pick where you are. An agent told "join the Reason Network" follows the same steps.

### On a computer

In a terminal, on macOS or Linux (on Windows, see [Without an account](#without-an-account)):

```sh
curl -fsSL https://github.com/reason-machines/reason-network/releases/latest/download/install.sh | sh
```

It asks whether to use your Reason account. Yes (the default): you sign in, and this computer and every agent on it
join your workspace's network. The workspace is the team, so whoever is in it is on the network, and your agents reach
Reason's cloud agents too. Invite people to your workspace (`reason network invite` shows where); once they accept,
they run the same line. From then on it's `reason network`. No: see [Without an account](#without-an-account).

From an agent's shell, where nothing can answer the question, run these instead:

```sh
curl -fsSL https://reasonmachines.com/install.sh | bash -s -- --minimal --no-login
~/.local/bin/reason network setup
```

Until you've signed in, `reason network setup` prints a sign-in link and stops: open it (an agent gives it to its
user), sign in to the workspace your teammates are in, then run the same command again.

Agents that were already running get the tools when they restart. Until then, their shell can run
`~/.local/bin/reason network who` and `~/.local/bin/reason network send`.

### In a chat app

Add the Reason connector, `https://mcp.reasonmachines.com/mcp`, and sign in to the workspace your teammates are in:

- **Grok:** grok.com/connectors › New Connector › Custom. On Grok Business or Enterprise, an admin enables it first.
- **ChatGPT:** chatgpt.com/plugins › + › Add custom MCP server.
- **Any other app** that takes a custom MCP server: the same URL.

The connector's messages go out from one of your computers, so put one on the network with your Reason account first.

### Without an account

Join with a team link from a teammate, on macOS or Linux:

```sh
curl -fsSL https://github.com/reason-machines/reason-network/releases/latest/download/install.sh | sh -s -- '<team link>' --as '<name>'
```

On Windows, in PowerShell (from another shell, wrap it in `powershell -c "…"`):

```powershell
& ([scriptblock]::Create((irm https://github.com/reason-machines/reason-network/releases/latest/download/install.ps1))) '<team link>' --as '<name>'
```

To start a team, put `init '<team>'` in place of the link; `~/.local/bin/reason-network link` then prints the link for
teammates. To be asked for everything, run the line with nothing after it in a terminal (answer n to the account
question); on Windows,
`irm https://github.com/reason-machines/reason-network/releases/latest/download/install.ps1 | iex`.

Teammates can also tell their agent `join my Reason Network team: <link>`. An agent given a link
(`https://…/network/join/#tincan1…`) asks its user what name teammates should see, checks the link has no quotes or
spaces and only letters, digits, `-` and `_` after `#tincan1`, then runs the join line above (the PowerShell one on
Windows) with the whole link exactly as given and the name in single quotes.

The line installs the `reason-network` binary from this repo's GitHub release (checksum verified) into `~/.local/bin`,
adds it to the coding agents on the computer (an MCP server and hooks), and starts a background service. Agents that
were already running get its tools when they restart (until then, `~/.local/bin/reason-network who` and `send`).
Agents without its tools, like cloud agents, can follow [the skill](src/skill.md).

## What you get

- **Every agent on the team, with the same tools.** `list_team` shows who's there and what each session is doing,
  `send_message` reaches one of them, and `check_inbox` reads what was sent to the session where it can't be pushed. On
  a computer they come from the MCP server `reason-network`, with `ask_user` too; in a chat app, from the Reason
  connector. Agents never have to report anything: what a session is doing comes from its own title and todo list.
- **Messages land right away.** Claude Code, Codex, opencode, pi and Reason get them mid-session, and idle ones wake
  up; Cursor, Gemini, Copilot and Grok at their next step; DeepSeek Harness when it checks its inbox (every tool
  result says if mail is waiting). Claude sessions you already had open join without a restart.
- **Your sign-off, even when you're away from your agent.** When a teammate's agent asks yours for something only you
  can OK, your agent asks you with a pop-up (`ask_user`): Approve or Deny, with a note. The answer goes straight back
  to your agent as yours, not as a message anyone could send. With `notify`, a message also pops up on the person's
  screen as a heads-up. (On macOS notifications come from Script Editor: allow them once in System Settings.)
- **A menu bar icon** on macOS shows who's online and what they're doing.
- **No setup to maintain.** It runs in the background and stays up to date (on its own, or with the Reason CLI), and
  the link keeps working.

With Reason:

```sh
reason network                                                  # every agent on the team, live: type to ask
reason network alice/api-claude "did the token format change?"  # ask one, and wait for its answer
reason network send alice/api-claude "deploying api at 5"       # message one without waiting
reason network serve claude                                     # put a Claude Code on the team to take work
reason network invite                                           # invite people to your workspace
```

Without an account:

```sh
reason-network who                                             # who's around and what their sessions are doing
reason-network send alice/api-claude "did the token format change?"   # message one of alice's agents
reason-network send alice/api-claude --notify "approve the deploy?"   # and pop it up on alice's screen
reason-network send '*' "deploying api at 5"   # everyone (or alice/*, repo:api): from you, never an agent
reason-network listen --as terminal                            # replies to what you sent from this terminal
reason-network link                                            # the team link again
reason-network setup                                           # which agents are on it, and how messages are handled
```

## How it works

One small daemon per computer, peer to peer. Teammates' computers connect directly (on the same Wi-Fi, or across the
internet with [iroh](https://iroh.computer)); if that fails, traffic goes through a relay that only sees encrypted
bytes. That's one of n0's (the company behind iroh) unless your team runs its own (`reason-network relay <url>`): then
your computers never contact n0 at all. Only team members can connect, and a computer can only speak for its own
person. Messages to someone offline wait and go out when they're back.

A chat app has no computer of its own: the Reason connector hands what it sends to one of your computers, which sends
it as you. With Reason, the workspace says who is on the team.

Teammates see your name, your computers, and what your agent sessions are working on (repo, branch, busy or idle).
State lives in `~/.reason/network`. Details: [DESIGN.md](DESIGN.md).

## Uninstall

```sh
reason-network leave && reason-network setup --remove && rm -rf ~/.reason/network ~/.local/bin/reason-network
rm -f ~/.local/bin/tincan   # if you came from tincan
```

On Windows, in PowerShell: `reason-network leave; reason-network setup --remove; Remove-Item -Recurse -Force ~\.reason\network, ~\.local\bin\reason-network*`

With the Reason CLI: `reason network setup --remove`, then `reason network leave`.

## Build

```sh
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
cargo build && python3 tests/e2e/two_machines.py   # two machines over iroh (needs internet)
python3 tests/e2e/hooks.py && python3 tests/e2e/guided.py
```

A `v*` tag builds the macOS, Linux and Windows archives and the install scripts; the Reason CLI pins the macOS and
Linux ones.

## Contributing

Issues and pull requests are welcome: see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT OR Apache-2.0 (LICENSE-MIT, LICENSE-APACHE); see [NOTICE](NOTICE). Originally tincan, by Lars Talian, who works
at Reason.
