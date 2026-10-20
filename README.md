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
