# The bus

Underneath, a small bus on this machine carries messages between agents:
the review's roles and their bridges, and your own sessions. An agent's
address is `name@machine`. A Claude Code session with the plugin is an
agent, named after its repository, and messages reach it as it works. From
a shell:

| Command | Does |
| --- | --- |
| `acrbus send <to> "text"` | Send a request. `--reply-to <id>` answers one, `--no-reply` asks for nothing back, and `--progress` sends a progress note on a request you're still working on. |
| `acrbus call <to> "text"` | Send a request, wait for the reply, print it, and exit with its status. |
| `acrbus recv`, `acrbus inbox`, `acrbus ack <id>` | Take your next messages, list what's waiting, or mark one handled. |
| `acrbus agent add <name>`, `acrbus peers` | Subscribe an agent before its first session, and list who you can reach. |
| `acrbus status`, `acrbus tail` | The daemon's state, and its log as it happens. |

Messages are untrusted: an agent gets each one tagged with who sent it, and
is told a request never overrides its user's instructions.

| Variable | Means |
| --- | --- |
| `ACRBUS_HOME` | Keep everything, state and config, in this one folder. |
| `ACRBUS_AGENT` | The agent a session or command acts as, instead of one named after the repository. |
| `ACRBUS_CLIENT` | Marks a session as a worker for a client, for `/pipe`. |

## Doors

A door lets an agent that runs in another OS account, such as one that
answers other people's questions on a team server, take requests from
your agents here and answer them, and nothing else. It gets a Unix socket
of its own, and the daemon checks every connection's user: only the
account named gets through, as the agent named and no other.

```toml
[doors.open-source]
account = "grayboard-oss"
socket = "/Users/Shared/acrbus-doors/open-source/door.sock"
```

Through its door, the agent can:

- take its messages (`recv`, with `confirm`), list them (`inbox`), and mark
  one handled (`ack`);
- read its own part of a thread (`thread`);
- answer a request it was sent, once, to whoever sent it (`send` with
  `reply_to`). The answer is text, and can't ask anything back.

Everything else is refused: a new message, a message to anyone else, a
`/pipe` step, progress notes, blocking calls, listing agents or machines,
and every admin, bridge and review request. Only agents on this machine can
send to it; a message from another machine is turned down. On the daemon's
own socket, the agent's name can be sent to and looked at, but nothing
there can take its messages or speak as it, so only its door acts as it.

The daemon makes the socket, but not its folder, which the setup makes, as
root, once:

1. The folder belongs to the user the daemon runs as, and nobody else can
   write in it (the daemon refuses the door otherwise), for example mode
   `0700`.
2. The account may pass through it: on macOS,
   `chmod +a "user:grayboard-oss allow search" <folder>`; on Linux,
   `setfacl -m u:grayboard-oss:x <folder>`.
3. Every folder above it can be passed through by the account, as
   `/Users/Shared` can.

The socket is `0666` inside that folder: being able to reach it is the
setup's part, and the connecting user's ID is what lets it in. A door that
can't open says why in `acrbus tail`, and the bus runs without it.

The protocol is the daemon's own, JSON lines over the socket: one request
per line, each answered with `{"ok": true, "data": ...}` or
`{"ok": false, "error": ...}`. A door client needs these:

| Request | Does |
| --- | --- |
| `{"op":"recv","agent":"open-source","session":"","max":10,"wait_secs":600}` | Waits for messages. If some come, answer `{"op":"confirm"}` within 30 seconds, or they're handed over again. |
| `{"op":"send","agent":"open-source","to":"","body":"...","reply_to":"<id>"}` | Answers request `<id>` to its sender. |
| `{"op":"inbox","agent":"open-source"}`, `{"op":"ack","agent":"open-source","id":"<id>"}` | Lists what's waiting; marks one handled without answering. |
| `{"op":"hello"}` | The machine's name, its owner and the daemon's version. |

To check a door from the account, as root:
`printf '{"op":"hello"}\n' | sudo -u grayboard-oss nc -U <socket>`.

## Building on acrbus

A program can build on acrbus: it names itself, adds networks such as one
that reaches other machines, and runs the shared command line.
[tailbus](https://github.com/Skeptic-Dog-Software/tailbus) is one: acrbus
with a Tailscale network, in two files.

```rust
use acrbus::config::App;

const MINE: App = App {
    name: "mybus",
    label: "com.example.mybus",
    about: "My agent bus",
    network: "local",
    networks: &[],
    env: &["MYBUS"],
};

fn main() -> std::process::ExitCode {
    acrbus::config::set_app(&MINE);
    acrbus::cli::main()
}
```

A network is an `acrbus::network::Kind`: its name for `network` in
`config.toml`, the settings it reads there, and how to open it as an
`acrbus::network::Network`.
