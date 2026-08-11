# Local tmux Architecture

## Decision

Use the user's actual local tmux as the interactive frontend. Keep a private
remote tmux server only as an invisible persistence engine. Do not copy the
local tmux configuration to the server and do not expose the remote tmux
prefix, status bar, copy mode, or key tables.

```text
Ghostty/Kitty
    |
local tmux using the user's real configuration
    |
local tmux pane running an ssht worker
    |
SSH or Mosh
    |
temporary prefixless remote tmux view session
    |
persistent remote tmux window containing one PTY
    |
remote shell/application
```

## Remote Model

Run a dedicated server on a separate socket:

```sh
tmux -L ssht -f /dev/null
```

Normal remote tmux continues to use the `default` socket. The private socket is
already isolated by remote UID, so it neither loads nor modifies another
user's tmux configuration.

Each workspace is one persistent session. Each remote window contains exactly
one pane. Local tmux owns all windows, splits, layouts, bindings, status, and
copy mode.

Spawn pane commands without exposing the backend tmux identity:

```sh
env -u TMUX -u TMUX_PANE "${SHELL:-/bin/sh}" -l
```

Store a stable generated PTY ID as a remote window user option. Do not persist
tmux window or pane IDs as durable identities.

## View Sessions

Each transport connection creates a temporary session grouped with the
persistent workspace, selects one backend window, and disables all visible or
interactive remote tmux UI:

```sh
tmux -L ssht new-session -d -t WORKSPACE -s VIEW_ID
tmux -L ssht set-option -t VIEW_ID status off
tmux -L ssht set-option -t VIEW_ID prefix None
tmux -L ssht set-option -t VIEW_ID prefix2 None
tmux -L ssht set-option -t VIEW_ID mouse off
tmux -L ssht select-window -t VIEW_ID:WINDOW
tmux -L ssht attach-session -t VIEW_ID
```

The view session may be deleted when its transport exits. The grouped backend
workspace retains the windows, PTYs, processes, screen state, and history.
Periodically remove stale unattached view sessions after hard disconnects.

## Local Model

Outside tmux, create a normal local session so the user's existing local tmux
server and configuration are used. Inside tmux, create and switch to a new
host-qualified session. Only that session receives ssht-specific session
options; existing local sessions remain untouched.

Set the workspace session's `default-command` to a hidden pane worker:

```sh
ssht __pane-worker --host HOST --workspace WORKSPACE --create
```

Normal user bindings for `new-window` and `split-window` then start workers.
Each worker atomically creates or locates one persistent remote window and
attaches to it through a temporary view session.

Store mappings as local pane options:

```text
@ssht_remote_pty
@ssht_host
@ssht_workspace
```

## Workspace Supervisor

Use one supervisor per local workspace for the SSH ControlMaster, configured
port forwards, credential reuse, topology snapshots, and worker lifecycle.
Pane workers must not each create independent masters or forwards. Mosh workers
use Mosh for terminal transport while the SSH sidecar retains forwarding and
control operations.

## Restoration

Snapshot local windows, panes, layouts, and stable remote PTY IDs after topology
changes. On another machine, query remote PTY IDs, create local windows and
splits, substitute newly created local pane IDs into the saved layout, and
start one worker per remote PTY. Fall back to one local window per persistent
PTY when no layout manifest exists.

## Terminal Policy

Local tmux is the Ghostty/Kitty compatibility boundary. The remote side normally
sees `TERM=tmux-256color`; ssht installs missing terminfo before attachment.
The private backend uses `tmux-256color`, extended keys, and deliberate terminal
passthrough. The heuristic local-echo relay is not part of this architecture;
use raw SSH or Mosh prediction.

## Implementation Boundaries

Split the current connection path into these modules:

```text
target.rs       resolved address, username, vault, and SSH arguments
transport.rs    SSH, Mosh, ControlMaster sidecar, and reconnect
workspace.rs    local tmux orchestration, workers, and topology snapshots
backend.rs      persistence backend interface
backend/tmux.rs private remote server, windows, views, and registry
terminfo.rs     local export, remote probe, installation, and fallback
```

Add hidden commands for the workspace supervisor, pane worker, snapshot, and
view cleanup. Replace the current remote attach command and alternate-screen
watcher only after the new path reaches feature parity.

## Delivery Phases

1. Prove one local pane attached to one invisible persistent remote window over
   SSH and Mosh. Verify resize, Vim, mouse, clipboard, Ghostty, and Kitty.
2. Add local splits and windows through `default-command`, plus stable PTY IDs.
3. Add the supervisor, one ControlMaster, forwarding, reconnect, and stale view
   cleanup.
4. Add same-machine and cross-device topology restoration.
5. Keep existing remote tmux attach as an explicit compatibility mode, then
   change the default only after real-host testing.

## Acceptance Criteria

- Only the local user's prefix, status, bindings, plugins, and copy mode appear.
- Remote user configuration and normal remote tmux sessions remain untouched.
- Different local clients may use different local configurations.
- Closing a local pane detaches without killing its remote process.
- SSH and Mosh preserve remote PTYs without exposing remote tmux UI.
- Ghostty and Kitty work without the local-echo relay.
- Port forwards and authentication are established once per workspace.
- Explicit local and remote kill operations remain distinct.

## Known Constraints

- A shared remote PTY has one canonical size and input stream.
- Simultaneous writers require an ownership policy.
- Advanced graphics still cross two tmux renderers and depend on passthrough.
- Local `pane_current_command` sees the ssht worker; synchronized pane titles can
  expose the remote foreground command separately.
- Exact cross-device layout restoration requires synchronized manifests.
