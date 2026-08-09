# Typing latency over SSH

Research notes and a working prototype, investigating whether `ssht` can make
typing over a high-latency link feel responsive.

**Status:** prototype built and tested (`demo/localecho.py`). No decision made
on shipping it. The deciding experiment is in [Before building anything](#before-building-anything)
and takes two minutes.

---

## The problem

SSH is a byte pipe. The remote PTY has `ECHO` enabled, so the path for a single
keystroke is:

```
keypress → encrypt → ½ RTT → remote PTY echoes → ½ RTT → character appears
```

Every character you type costs a full round trip before you see it. At 20ms
nobody notices. At 120ms — transatlantic, LTE, or a bastion hop — typing feels
like wading.

Nothing in the transport layer changes this. The RTT is the RTT. The only way
out is for the client to draw something before the server answers, which means
the client has to know, or guess, what the server would have drawn.

---

## Prior art

### Mosh — the only mature solution

Mosh runs a terminal emulator on **both** ends. `mosh-server` holds authoritative
screen state; the client keeps its own copy and syncs diffs over UDP (SSP, the
State Synchronization Protocol). Because the client knows the cursor position
and screen contents, it can hypothesise: *you typed `k` at column 12, so `k`
probably appears at column 12.*

The man page describes the model precisely:

> The client runs a predictive model in the background of the server's behavior,
> hypothesizing that each keystroke will be echoed at the cursor location and
> that the backspace and left- and right-arrow keys will have their traditional
> effect. […] On longer-latency links, the predicted cells are underlined until
> confirmed by the server.

The prediction is **validated, not blind** — that is the whole design. When a
guess is falsified, mosh ends the prediction epoch and goes quiet until the next
confirmed sync. Worst case is "behaves like SSH," never "shows you garbage."

Where it lands:

| Context | Prediction works? | Why |
| --- | --- | --- |
| Shell line editing, `nano` | Yes — feels local | Printable char at cursor is exactly the modelled case |
| `vim` insert mode | Yes | Same case. Add `--predict-overwrite` for replace mode |
| `vim` normal mode | **No, and unfixably so** | `dd` deletes a line, `ciw` changes a word. Predicting the screen effect requires modelling vim's state machine |

Costs, honestly stated:

- `mosh-server` binary on the remote (userland install, no root needed)
- UDP 60000–61000 must be reachable
- No port forwarding, no agent or X11 forwarding
- **No scrollback** — see [Conflict with ssht's scrollback design](#conflict-with-sshts-scrollback-design)

### Termius — verified: it just embeds mosh

Worth checking, since it's the polished commercial option. It does nothing of
its own. It ships a built-in Mosh client, toggled per host (Desktop: Host
Details → "Show more" → Mosh; Mobile: Edit Host → "Use Mosh"), then SSHes in,
spawns `mosh-server`, and hands off to a UDP session — the same bootstrap the
`mosh` wrapper script performs.

Requirements are identical to plain mosh, and their docs say so:

> The Mosh service is distinct from SSH and must be installed and configured
> separately on your host system.

Mosh 1.3.0+, with custom port ranges via
`mosh-server new -s -l LANG=en_US.UTF-8 -p [from]:[to]`.

The one genuine convenience: no local mosh **client** binary needed, since it's
compiled into the app. That's packaging, not protocol. There is no separate echo
engine, so Termius inherits mosh's ceiling exactly — including no prediction in
vim normal mode.

Note also that Termius's snippets and autocomplete are local UI conveniences.
They make you type fewer characters; they don't make characters appear faster.
Easy to conflate when a client feels snappy.

### Telnet LINEMODE / PuTTY — the older model

Before prediction, the answer was **actual local line editing**: the client owns
the line, and sends it whole on Enter. No guessing at all — genuinely instant,
zero uncertainty.

Telnet could do this because it had explicit ECHO/SGA option negotiation
(RFC 857/858): the server announced who was doing the echoing. **SSH has no such
channel.** That's why PuTTY's "local line editing: Auto" resolves to *off* for
SSH sessions. This gap is the crux of the whole investigation — see
[The key finding](#the-key-finding).

### Everything else

| Thing | Fixes keystroke latency? |
| --- | --- |
| Eternal Terminal (`et`) | No. Persistence and roaming over TCP, keeps port forwarding, but **no predictive echo** |
| Blink Shell (iOS) | Only via mosh — same story as Termius |
| `ControlMaster` / `ControlPersist` | No. Kills *connection setup* cost only. Already used in `src/mux.rs:121` |
| Cipher choice (`aes128-gcm`, `chacha20`) | No. Microseconds on any modern CPU |
| `Compression yes` | Actively worse — adds CPU latency to tiny packets |
| `TCP_NODELAY` | Already set by OpenSSH |
| tmux `-CC` control mode, WezTerm mux server | No. Structured protocol, still one RTT per keystroke |

### Things that do shave real milliseconds

- **Drop `ProxyJump` hops.** Each bastion adds a full RTT leg. Often the single
  biggest win, and easy to forget you're paying it.
- **`set -sg escape-time 10`** in tmux (plus vim's `set ttimeoutlen=10`). The
  500ms default makes `ESC` feel broken. This is *perceived* responsiveness and
  it's the most common complaint misattributed to network lag. Worth shipping in
  ssht's tmux setup regardless of anything else in this document.
- **Ethernet over WiFi.** Power-save adds 10–50ms of jitter, which reads as worse
  than steady latency.

### The architectural fix for editors

Stop running the editor remotely; run the **UI** locally and sync state.

- **`nvim --headless --listen` + local `nvim --remote-ui`** over a forwarded
  socket. Typing is fully local, only buffer state crosses the wire. Genuinely
  better than mosh for nvim users.
- **VS Code Remote / Zed remote / JetBrains Gateway** — whole-IDE version.
- **sshfs / `vim scp://` / Emacs TRAMP** — files come to you, but you lose the
  remote's LSP, ripgrep, and build tools.

---

## Before building anything

Two things commonly make mosh feel like plain SSH, and **Termius exposes
neither**. Rule these out before writing code.

1. **Mosh defaults to `--predict=adaptive`** — it only predicts when it
   *measures* latency high enough to bother. At 40–80ms it often doesn't engage
   at all. Termius's toggle is on/off with no prediction-mode setting.

2. **A fancy prompt actively defeats prediction.** Powerlevel10k, starship, and
   especially `zsh-syntax-highlighting` / `zsh-autosuggestions` repaint the
   *entire line* with colour codes on every keystroke. Mosh's model is "the char
   you typed appears at the cursor"; a full-line repaint falsifies that
   instantly, so mosh kills the epoch and goes silent. On a themed zsh you may be
   getting almost no prediction at any latency.

The experiment, from a plain terminal:

```sh
mosh --predict=experimental you@host -- env -i PS1='$ ' /bin/sh -i
```

- **Types instantly, but Termius doesn't** → the problem is configuration, not
  protocol. Nothing to build.
- **Still laggy** → something more interesting is happening; measure where.

---

## Evaluating the local-buffer idea

The proposal: keep a local buffer, display it immediately, and push it to the
server at whatever speed the connection manages.

**The load-bearing assumption is that the screen is a function of your
keystrokes. It isn't.** Tab completion rewrites the line. Ctrl-R replaces it.
Autosuggestions append grey text you didn't type. A password prompt echoes
*nothing*. Vim normal mode turns `dd` into a deleted line.

So displaying the local buffer means displaying a **guess**. No amount of
decoupling send-rate from display removes the guess — it only determines how
long you look at a wrong screen before it's corrected. Prediction is the easy
half; reconciliation is the whole problem, which is why mosh predicts *echo*
specifically and reconciles against authoritative state.

The decoupling half also solves a non-problem: typing is ~10 chars/sec, a few
hundred bits. You are never bandwidth-bound on input. The cost is the round trip,
and buffering doesn't shorten it.

**But there is a real version.** When you *know* the remote is doing ordinary
canonical-mode line editing, you don't predict — you take over line editing
entirely and locally. That's the LINEMODE model, and the only reason nobody does
it over SSH is that you can't tell when you're in that state.

---

## The key finding

You can't ask, but you can **watch**. Since ssht proxies the byte stream,
alternate-screen entry is visible in-band:

```
\e[?1049h   enter   (also legacy \e[?1047h, \e[?47h)
\e[?1049l   exit
```

That's an instant, reliable, zero-round-trip signal for "vim/htop/less just took
over" — the same signal `scrollback_replay` already relies on via tmux's
`alternate_on` (`src/tmux.rs:38`), but observed locally instead of asked for.

**A tmux-based alternative was investigated and rejected.** tmux 3.2+ offers
format subscriptions via `refresh-client -B name:what:format`, pushing
`%subscription-changed` notifications in control mode — which would give richer
state than alt-screen alone (`pane_current_command`, `pane_in_mode`). Checked
against the tmux 3.7b man page:

> After a subscription is added, changes to the format are reported with the
> `%subscription-changed` notification, **at most once a second**.

A full second of staleness about whether you're in vim is far worse than the RTT
being hidden. Scrap it; parse the stream locally.

---

## The prototype

`demo/localecho.py` — 357 lines, stdlib only, no server component.

Outside the alternate screen it takes over line editing completely: keystrokes
paint locally and instantly, and **nothing goes over the wire until Enter**. On
`\e[?1049h` it drops to raw passthrough. This is deliberately *not* mosh — no
prediction, no server-side emulator, and no benefit inside vim, which is the
honest ceiling for a client-only approach.

```sh
./demo/localecho.py --latency 150          # local shell + synthetic RTT
./demo/localecho.py --ssh myhost           # real host, real latency
./demo/localecho.py --ssh myhost --latency 100
```

**Ctrl-]** toggles local line editing live, so the same connection at the same
latency can be A/B'd. That comparison is the point of the demo.

### Echo suppression

The piece requiring the most care. Since the line is already painted locally, the
remote's echo of that same line must be swallowed or everything appears twice.
The implementation matches byte-for-byte and **abandons suppression on the first
mismatch**, so a syntax-highlighting zsh that repaints the line wins the conflict
instead of corrupting the screen. This is a miniature of mosh's reconciliation
problem, and the same self-healing principle applies.

### Bailout keys

Tab, Ctrl-R, ESC (and therefore arrows), Ctrl-A/E/B/F/K/P/N cannot be serviced
locally — they need remote state we don't have. Pressing one flushes the buffer,
hands the remote everything typed so far, and falls back to raw mode for the rest
of the line, re-arming on Enter.

---

## Results

Driven through a real PTY against a real shell, plus direct state-machine tests.
All passing:

| Test | Result |
| --- | --- |
| Line commits and echoes exactly once | ✅ |
| Backspace, Ctrl-U, Ctrl-W | ✅ |
| Alt-screen enter/exit detection | ✅ |
| Sequence split across two reads (carry buffer) | ✅ |
| Unsent input flushes when a full-screen app launches | ✅ |
| Raw passthrough inside alt screen (no buffering) | ✅ |
| Echo suppression | ✅ |
| Self-heal on highlighted repaint mismatch | ✅ |
| Tab bailout, then re-arm on Enter | ✅ |
| Ctrl-] live toggle | ✅ |
| UTF-8 multibyte erases as one character | ✅ |

**One real bug surfaced during testing.** The backspace handler popped the
trailing byte *before* checking for UTF-8 continuation bytes, so the lead byte
survived and `é` erased to a broken half-character. Fixed by discarding
continuation bytes (`0b10xxxxxx`) first, then the lead byte. Re-verified across
`é`, `日本`, `aéb`, `🎉`.

---

## Hazards

**Password prompts.** When the remote disables echo there is no way to know — no
channel exists over SSH. The prototype sniffs the output stream for
`assword` / `assphrase` / `PIN` / `Verification code` and disarms local echo for
the next line. This catches sudo/ssh/su, which is most of what matters in
practice, but **it is a heuristic, not a guarantee**. A prompt it doesn't match
will paint input on screen. Any shipped version needs a better answer than this.

**Running ahead.** "Push at whatever speed the connection manages" lets local
state get semantically *ahead* of the server. Type 40 characters into what you
believe is a prompt, and if the previous command errored, or a sudo prompt
appeared, or completion fired, those bytes get interpreted in a context never
seen. That is the mechanism by which people accidentally execute things. Mosh
deliberately never allows this: it predicts *display* only, and input still goes
at wire speed. The prototype holds input until Enter, which trades this hazard
for the Tab-completion limitation.

---

## Conflict with ssht's scrollback design

`src/tmux.rs:25-31` replays session history into the *host* terminal so
iTerm/kitty/Ghostty own it and native scrolling works — explicitly framed as the
thing mosh gets criticised for.

That criticism holds, and it holds against **every** mosh-based client including
Termius. Mosh's protocol never transmits lines that have scrolled off the
server's screen; they don't exist in the synced state. No client-side renderer
can recover them. This is protocol-level, not a rendering choice.

**The tension is structural:** ssht's differentiator is that the *local terminal*
owns the scrollback. Predictive echo requires the *client program* to own the
screen. Both can be offered as modes; they cannot coexist in one session.

The local-line-editing prototype has no such conflict — it never takes over the
screen, so `scrollback_replay` keeps working.

---

## Where this leaves things

Three options, not mutually exclusive:

1. **Ship `set -sg escape-time 10`** in the tmux session setup. Free, one line,
   fixes the most-reported "SSH feels laggy in vim" complaint that isn't actually
   the network. Do this regardless.

2. **`--mosh` as an opt-in flag.** Shelling out to `mosh --ssh="ssh -p …" host --
   tmux new -A -s ssht` is a small change in `connect.rs`, consistent with the
   existing "shell out to the real tool" philosophy. Requires gating
   `scrollback_replay` off when active, and keeping a parallel `ssh -N -L…` over
   the existing ControlMaster for port forwards, since mosh can't carry them.

3. **Port the local-line-editing prototype to Rust.** Bounded scope, no server
   component, no reconciliation engine, degrades to plain SSH rather than to a
   wrong screen. Needs a better password-prompt story first.

**Do not write a prediction engine.** Termius is a funded commercial product with
a custom terminal stack across five platforms and it embedded mosh rather than
build one. Blink did the same. The moment you want predictive echo you need a
server-side emulator to sync against — at which point you have rebuilt mosh and
inherited its scrollback loss.

**The deciding signal** comes from running the demo: if shell prompts feel
genuinely instant and the bailouts aren't jarring, option 3 is worth building. If
the Tab-completion flush is the thing you notice most, it isn't — and that's much
cheaper to learn now than after a Rust port.

---

## Sources

- [Termius docs — Connecting to a server](https://docs.termius.com/organize-and-connect-to-hosts/connecting-to-a-server)
- [mosh.org](https://mosh.org/)
- [mosh(1) man page](https://manpages.ubuntu.com/manpages/questing/man1/mosh.1.html)
- [Secure VPS Access: Tailscale, Termius, and Mosh](https://medium.com/@aisgandy/secure-vps-access-using-tailscale-termius-and-mosh-to-close-public-ssh-ports-ec47691716ae)
- `man tmux` (3.7b) — `refresh-client -B`, `%subscription-changed`, `alternate_on`
