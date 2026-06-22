# ssht

Managing a handful of remote servers is fine. Managing a dozen is where it falls apart.

Your connection details are scattered across `~/.ssh/config`, sticky notes, and muscle memory. You SSH into a box, get a long-running job going, then your laptop sleeps on the train and the connection drops — taking the shell, the job, and your scrollback with it. You start the same work on your desktop the next morning and none of the context came with you. And when you just want to hop onto "that one staging server," you're squinting at config files trying to remember whether you called it `stg` or `staging-2`.

Terminal emulators don't fix any of this. They're local tools. They forget everything the moment you disconnect, because remembering was never their job.

## What ssht does

`ssht` turns every SSH connection into a persistent tmux session. You connect, you disconnect, you reconnect from a different machine — and the session is exactly where you left it, running jobs and all.

```
ssht prod-web
```

That's the whole interaction. It SSHes in, attaches to your existing tmux session if there is one, and creates it if there isn't. Nothing to set up on the remote beyond having `tmux` installed.

Run it with no arguments and you get a fuzzy-searchable picker built from the SSH config you already have:

```
ssht
```

Zero configuration required. If a host is in `~/.ssh/config`, it shows up in the picker. Each row tells you when you last connected, how many times, and — checked live in the background — whether a tmux session is currently running on that host. Server management stops being a chore.

## How it's built

I built this because I bounce between a laptop, a desktop, and the occasional borrowed machine, and I was tired of losing my place every time. I wanted something that did one thing well and got out of the way.

It's written in Rust, and a few choices were deliberate:

- **It shells out to the system `ssh` binary instead of reimplementing the SSH protocol.** This is the most important decision in the project. Your `ProxyJump`, your `IdentityFile`, your per-host `Port`, your `Match` rules, your hardware keys — all of it already works, because the real `ssh` is doing the connecting. ssht just hands it the right arguments and lets it do its job. Reimplementing SSH would have meant a larger attack surface, a maintenance burden, and subtle incompatibilities with the config you've spent years tuning. Not worth it.

- **The SSH config parser is written from scratch** to correctly handle `Include` directives (with globbing and recursion), `Match` blocks, and wildcard `Host` entries. This is the part most similar tools get wrong — they do a naive line scan, miss your `Include ~/.ssh/config.d/*`, and silently drop half your hosts. ssht follows includes, skips pure-wildcard entries like `Host *` so they don't pollute the picker, and falls back to `~/.ssh/known_hosts` for anything not in your config.

- **SQLite (via `rusqlite`) for local state.** Last-connected times, connection counts, and notes live in a single embedded database file. No daemon, no server, no JSON file to corrupt on a half-written exit.

- **`ratatui` for the TUI and `nucleo` for fuzzy matching.** nucleo is the same matching engine behind some of the fastest finders around; it makes the picker feel instant even with a long host list.

- **`tokio` for async background checks.** When the picker opens, the host list is already on screen — it doesn't wait on the network. The "is tmux running over there?" probes fire off concurrently in the background and the indicators fill in as answers come back. Opening the picker never blocks on a slow or unreachable host.

The result is a single static binary with no runtime dependencies beyond `ssh` and `tmux` being on your `PATH`.

## Installation

You need `ssh` (locally) and `tmux` (on the remote hosts) available on your `PATH`. Then build from source:

```
git clone https://github.com/ayaanhafeez/ssht
cd ssht
cargo install --path .
```

Or build the binary directly and drop it wherever you keep your tools:

```
cargo build --release
cp target/release/ssht ~/.local/bin/
```

## Usage

### `ssht` — interactive picker

Launch with no arguments for the fuzzy-searchable TUI:

```
ssht
```

Start typing to filter. Each entry shows the alias, the resolved endpoint, when you last connected, your connection count, and a live tmux indicator (`●` means a session is currently running on that host).

| Key | Action |
| --- | --- |
| type | fuzzy filter |
| `↑` / `↓`, or `Ctrl-p` / `Ctrl-n` | move selection |
| `Enter` | connect |
| `Ctrl-u` | clear the query |
| `Esc` / `Ctrl-c` | quit |

### `ssht <host>` — direct connect

If you know where you're going, skip the picker:

```
ssht prod-web
ssht staging-2 --layout dev          # apply a named layout on first attach
ssht prod-web -- -L 8080:localhost:80   # everything after -- goes straight to ssh
```

Anything after `--` is passed verbatim to the underlying `ssh` invocation, so port forwarding, agent forwarding, and one-off options all work without ssht needing to know about them.

### `ssht list` — scriptable output

Prints every known host, one per line, with no decoration — meant for piping:

```
ssht list
ssht list | fzf | xargs ssht
ssht list | grep prod
```

### `ssht last` — reconnect to the most recent host

For when you got disconnected and just want to get back in:

```
ssht last
```

### `ssht edit` — open the config

Opens `~/.config/ssht/config.toml` in `$EDITOR` (falling back to `$VISUAL`, then `vi`), creating a starter file if you don't have one yet:

```
ssht edit
```

## Configuration

ssht works with no config at all. The optional config file at `~/.config/ssht/config.toml` (it honors `$XDG_CONFIG_HOME`) is only for ssht-specific metadata you can't express in `~/.ssh/config` — custom session names, layouts, and notes.

```toml
[settings]
# tmux session name used for any host that doesn't override it below
default_session = "main"

# Per-host metadata. Keys are the ssh aliases from ~/.ssh/config.
[hosts.prod-web]
session = "web"                 # use a tmux session named "web" instead of "main"
layout  = "dev"                 # apply the "dev" layout the first time the session is created
notes   = "primary web server — handle with care"

[hosts.db-primary]
session = "ops"
notes   = "postgres 16, replicas in eu-west"

# Layouts describe an ordered set of tmux windows, applied only when the
# session is first created. Reconnecting to an existing session just attaches.
[[layouts.dev.windows]]
name = "editor"
command = "nvim"                # optional: run a command in the window on creation

[[layouts.dev.windows]]
name = "logs"
command = "journalctl -fu app"

[[layouts.dev.windows]]
name = "shell"                  # no command — just an empty shell
```

The `notes` show up in the picker, the `session` name controls what tmux session you land in, and `layout` lets you walk into a fully-arranged workspace the first time you connect.

## How the tmux integration works

The persistence comes down to one tmux flag. On connect, ssht runs roughly this on the remote:

```
tmux new-session -A -s <session>
```

The `-A` flag means **attach if the session exists, otherwise create it**. So the first connection creates the session; every connection after that attaches to the same one. When your network drops, tmux keeps the session (and everything running in it) alive on the server. The next `ssht <host>` — from any machine — drops you right back into it.

When a layout is configured, ssht generates a slightly longer command that checks whether the session already exists; if not, it builds the windows and runs their commands before attaching. If the session is already there, it skips all of that and just attaches, so your layout is never re-applied on top of work in progress.

## Roadmap

Honest about what isn't here yet:

- **Session sharing** — read-only or collaborative attach to the same remote session for pairing.
- **Port forwarding UI** — defining and toggling forwards from the picker instead of passing `-L`/`-R` by hand.
- **Teams / sync** — sharing host metadata and layouts across a team, or syncing your own state between machines.
- **Connection health in `list`** — surfacing the live tmux indicator in the scriptable output, not just the TUI.
- **Per-host pre/post hooks** — running a local command before connecting or after disconnecting.

Contributions and ideas are welcome.

## License

MIT. See [LICENSE](LICENSE).
