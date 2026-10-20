# Claude Agent Mascot — Rust edition

Same mascot, same protocol, same hooks — firmware and host tooling both in
Rust instead of Arduino C++ / Python. See the original `claude-mascot/`
project for the concept; this is a straight port.

```
firmware/   no_std embedded crate for the Wio Terminal (ATSAMD51)
host/       two Rust binaries: mascot-daemon (UDP -> serial) and mascot-hook
```

## Honesty check before you build

The firmware targets the `wio_terminal` BSP crate (v0.9.0) and was written
against its own published examples (blinky/buttons/usb_serial_display,
pulled from the crate's source) plus its exact pinned dependency versions
(`usb-device 0.3`, `usbd-serial 0.2`, `heapless 0.8`, `embedded-graphics
0.8.1`, `atsamd-hal 0.21`). That said, embedded Rust APIs — especially
`usb-device`'s device-builder — have changed shape multiple times across
versions, and this was written without a compiler in the loop against your
exact toolchain. Expect it to need one or two small fixes on first build,
most likely around the USB descriptor builder call. The "If it doesn't
compile" section below covers the likely spots.

## 1. Build and flash the firmware

```bash
cd firmware
rustup target add thumbv7em-none-eabihf
cargo build --release
```

Put the Wio Terminal in bootloader mode (double-tap the reset switch on the
bottom of the device — the orange LED will pulse), then flash with either:

```bash
# Option A: bossac
arm-none-eabi-objcopy -O binary \
  target/thumbv7em-none-eabihf/release/claude-mascot-fw \
  target/thumbv7em-none-eabihf/release/claude-mascot-fw.bin
bossac -i -d -v --port=<PORT> -U -i --offset=0x4000 \
  -w -v target/thumbv7em-none-eabihf/release/claude-mascot-fw.bin -R

# Option B: cargo-hf2 (one step)
cargo install cargo-hf2
cargo hf2 --release --vid 0x2886 --pid 0x002d
```

## 2. Build the host binaries

```bash
cd host
cargo build --release
mkdir -p ~/.claude-mascot
cp target/release/mascot-hook ~/.claude-mascot/
```

Run the daemon whenever you're using Claude Code:

```bash
./target/release/mascot-daemon                # auto-detect port
./target/release/mascot-daemon /dev/ttyACM0    # or explicit
```

### Run the daemon as a service (starts at boot)

A systemd *user* service keeps the daemon running and restarts it if it exits
(e.g. the board wasn't plugged in yet). Run from the repo root:

```bash
cd host && cargo build --release && cd ..
install -m 755 host/target/release/mascot-daemon ~/.claude-mascot/
mkdir -p ~/.config/systemd/user
cp host/mascot-daemon.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now mascot-daemon
```

To start at boot without logging in, enable lingering once:
`sudo loginctl enable-linger $USER`.

Check it with `systemctl --user status mascot-daemon` and
`journalctl --user -u mascot-daemon -f` (look for `connected to ...`). After
rebuilding, re-run the `install` line and `systemctl --user restart mascot-daemon`.

Notes:
- Your user needs access to the serial port: `sudo usermod -aG dialout $USER`
  (or `gpasswd -a`), then log out and back in. With lingering enabled, reboot or
  `sudo systemctl restart user@$(id -u)` so the service manager picks up the group.
- The unit passes `/dev/serial/by-id/usb-Claude_Mascot_Claude_Agent_Mascot_MASCOT1-if00`
  explicitly because the firmware's USB name ("Claude Agent Mascot") no longer
  matches the daemon's auto-detect hints. Check yours with `ls /dev/serial/by-id/`.
- Don't run a manual `mascot-daemon` at the same time; it holds the port and the
  UDP socket (`127.0.0.1:8765`).

### herdr integration (optional)

If you manage your agents with [herdr](https://herdr.dev), the daemon shows all
of them on the board as a row of dots under the mascot, one per agent. It
subscribes to herdr's event socket (`~/.config/herdr/herdr.sock`, or
`$HERDR_SOCKET_PATH`), so there is no polling and only read-only calls are made.
herdr is optional: while it isn't running the daemon retries quietly (1s backing
off to 10s, one log line per change) and everything else works as before.

| herdr status | dot |
|---|---|
| `blocked` (waiting for you) | red |
| `working` | green |
| `idle` / `done` | yellow |
| `unknown` | gray |

The daemon sends one serial line with the full state, only when it changes (and
again after every serial reconnect, since the board forgets it when it resets):

```
AGENTS:BBIIIIIIW      # B blocked, W working, I idle/done, U unknown; one char per agent
AGENTS:               # no agents
```

Agents keep their position (order of first sight, max 32), so dots don't jump
around as statuses change. To check what would be sent without touching the
board, run `mascot-daemon --herdr-dry-run`: it prints the `AGENTS:` lines to
stdout and opens neither the serial port nor the UDP socket, so it is safe to run
next to the service.

## 3. Wire up the hooks

Merge `claude_settings_hooks.json` into `~/.claude/settings.json` the same
way as the C++/Python version — see the original project's README for the
event-by-event behavior table. Restart Claude Code after editing.

## If it doesn't compile

**USB descriptor builder (`UsbDeviceBuilder::new(...).strings(...)`)** —
this is the single most likely spot to need adjusting; `usb-device`'s
builder API has been reshuffled a few times:
- If your resolved version wants `.manufacturer("..").product("..").serial_number("..")`
  chained directly on the builder instead of `.strings(&[StringDescriptors...])`,
  that's the older 0.2-style API — swap it in.
- If `UsbDeviceBuilder::new()` complains about a missing third argument, your
  version wants an explicit `&mut [u8; N]` control buffer — add one, e.g.
  `let mut control_buf = [0u8; 128];` and pass `&mut control_buf` as the
  third argument.

**`RoundedRectangle::with_equal_corners`** — if this constructor isn't
found, build the `CornerRadii` explicitly instead:
```rust
use embedded_graphics::primitives::CornerRadii;
RoundedRectangle::new(body_rect, CornerRadii::new(Size::new(40, 40)))
```

**`Queue::new()` on a `static mut`** — if this isn't a `const fn` in your
resolved `heapless` version, check that crate's changelog for the
version-appropriate static-initialization form; the queue's job here
(buffering incoming serial bytes between the USB interrupt and the main
loop) is unaffected by which form you use.

**Chip/clock setup** — `GenericClockController::with_external_32kosc(...)`
and the `sets.display.init(...)` call are copied verbatim from the crate's
own examples, so these are the parts least likely to have drifted.

If you get stuck, `cargo doc -p usb-device -p wio_terminal -p heapless
--open` opens the exact API surface for the versions Cargo actually
resolved for you, which is more reliable than anything written here.
