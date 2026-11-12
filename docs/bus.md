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
