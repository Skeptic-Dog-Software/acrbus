# Bridges

A bridge answers its agent's messages by running a headless agent CLI once
per message and sending back what it prints. Configure one in
`config.toml` and start it with `acrbus bridge install <agent>`:

```toml
[bridges.coder]
owner = "you@example.com"
command = "/usr/local/bin/your-agent"
args = ["--print", "--session-dir", "{SESSION_DIR}"]
working_directory = "/Users/you/agent-work"
home = "/Users/you/agent-home"
path = "/usr/local/bin:/usr/bin:/bin"
```

| Setting | Means |
| --- | --- |
| `kind` | `"claude"`, `"omp"` or `"pi"`: an agent acrbus ships defaults for. See [Another agent](quickstart.md#another-agent). |
| `owner` | Who a message must come from: the bus's `owner`. Required. |
| `command`, `args` | What to run. `args` can use the placeholders below. |
| `extra_args` | More arguments, after `args`: what this machine adds to its kind's defaults, such as a provider's extension. |
| `working_directory` | Where it runs. Required. |
| `home`, `path` | Its `HOME` and `PATH`. Required. `home` can't be your own. |
| `session` | `"fresh"`, the default, gives every request a new session. `"per-sender"` keeps one conversation with each sender. |
| `max_concurrent` | The most runs at once. Default 1. |
| `timeout_secs` | How long a run can take. Default 600. A review's call asks for as long as the bridge allows, `max_timeout_secs` or this, so give a review's bridge hours: `acrbus setup acr` writes 14400. |
| `idle_secs` | How long a run can go quiet before its process tree is woken, and, if nothing stirs, ended and tried again in a fresh run, up to 3 tries in all. Default 1200; 0 turns it off. Either way, `acrbus bridge status` shows when each running request last used CPU or changed a file, which tells a slow run from a stalled one. |
| `env`, `secrets` | More environment, and variables read from files only you can read. |
| `attachments` | `"path"`, the default, tells the agent where each attachment is. `"inline"` puts each one's text in its message, for an agent that can read only its working directory, since attachments are kept outside it. |
| `options.no_tools_args` | The arguments that run the agent with no tools at all, in place of `tools_args`, when a request asks for none: ACR's Sizer does, for its first look. pi's are `--no-tools --no-mcp`, and Claude Code's `--tools ""`. Left out, a request for no tools is refused. |
| `log_retention_days` | How long each run's archive is kept. Default 30. See [What's kept](#whats-kept). |

| Placeholder | Is |
| --- | --- |
| `{AGENT}`, `{ID}`, `{SENDER}` | The bridge's agent, the message's ID, and who sent it. |
| `{SESSION_DIR}` | The run's session directory, which keeps sessions apart. |
| `{RUN_DIR}`, `{MESSAGE_FILE}` | A directory for this run alone, and the file holding the message. |
| `{MODEL}`, `{TOOLS}`, `{EFFORT}` | What the request asked for, where `options` allows it. |

A bridge isn't a sandbox: the agent runs as you, with what its own settings
allow.

## What's kept

Nothing a run or a review leaves is deleted as it finishes, so you can see
what each agent did. Each finished thing is packed into a `.tar.gz` named
for its time, such as `20261112T134507Z-01M7…tar.gz`, so a listing reads in
time order, and an agent that searches files later doesn't come on an
earlier review's findings:

- each request a bridge takes, under
  `~/.local/state/acrbus/bridge/<agent>/runs/`: a folder for each try, with
  the agent's session, its scratch files and what it was sent, and the
  run's output and record. `acrbus bridge log <agent> <id>` reads it, with
  `--stdout` or `--stderr` for its output;
- a finished review's files, under `~/.local/state/acrbus/acr/`. It can't
  be resumed, but its record is there. One waiting for you stays unpacked,
  to go on from;
- a trial's or a check's copy, canary and sessions, and the findings kept
  for `acr feedback`, compressed.

The copies reviewers, the test gate, the doctor and the configurer work in
stay too, named for their time but not packed: each is a clone of the
repository that costs next to nothing on disk, and what a reviewer did in
it is in its packed session.

The daemon clears them once an hour, when they're 30 days old, or a
bridge's `log_retention_days`, going by the time in their names, and packs
anything finished that wasn't. Nothing else removes them. A run still open
stays however old, and kept findings stay 90 days. Everything is under the
state folder, which only you can read. A reviewer with a shell could still
unpack an archive, so ACR's system prompt tells it not to read what earlier
reviews and runs left.
