//! Claude Agent Mascot — Wio Terminal firmware (Rust, no_std)
//!
//! Same behavior as the Arduino/C++ version: reads plain-text command lines
//! over USB CDC serial (see ../../host/) and animates a blob mascot plus an
//! "agents running" counter.
//!
//! This is built directly against the `wio_terminal` BSP crate's own
//! examples (blinky/buttons/usb_serial_display, fetched from its source) so
//! the peripheral setup below matches its real API. The embedded Rust
//! ecosystem moves fast though — if something here doesn't compile against
//! the exact versions Cargo resolves for you, see the "If it doesn't
//! compile" section in ../README.md before assuming the whole approach is
//! wrong; it's almost always a one-line signature fix (most likely the USB
//! descriptor builder call near the bottom of `main`).
//!
//! Commands (newline-terminated), identical to the C++ version:
//!   SESSION_START | SESSION_END | PROMPT | AGENT_START | AGENT_STOP | STOP
//!   | NOTIFY:<msg> | PING
//!
//! Buttons: A mutes, B toggles a debug line, C pets the mascot.

#![no_std]
#![no_main]

use embedded_graphics as eg;
use panic_halt as _;
use wio_terminal as wio;

use eg::mono_font::{ascii::FONT_4X6, MonoTextStyle};
use eg::pixelcolor::Rgb565;
use eg::prelude::*;
use eg::primitives::Rectangle;
use eg::text::{Baseline, Text};

use ili9341::Orientation;

use cortex_m::peripheral::NVIC;
use wio::entry;
use wio::hal::clock::GenericClockController;
use wio::hal::delay::Delay;
use wio::hal::ehal::digital::OutputPin;
use wio::hal::usb::UsbBus;
use wio::pac::{interrupt, CorePeripherals, Peripherals};
use wio::prelude::*;

use usb_device::descriptor::lang_id::LangID;
use usb_device::device::{StringDescriptors, UsbDevice};
use usb_device::prelude::*;
use usbd_serial::{SerialPort, USB_CLASS_CDC};

use core::fmt::Write as _;
use heapless::spsc::Queue;
use heapless::String as HString;

// Approximate SAMD51 core clock after `with_external_32kosc`'s default
// setup. Only used to time the buzzer's bit-banged square wave, so it just
// needs to be roughly right, not exact.
const CPU_HZ: u32 = 120_000_000;

// ---------------- USB plumbing (interrupt-driven CDC serial) ----------------
// Structure mirrors wio_terminal's own `usb_serial_display` example.

static mut USB_ALLOCATOR: Option<usb_device::bus::UsbBusAllocator<UsbBus>> = None;
static mut USB_BUS: Option<UsbDevice<UsbBus>> = None;
static mut USB_SERIAL: Option<SerialPort<UsbBus>> = None;
static mut RX_QUEUE: Queue<u8, 256> = Queue::new();

fn poll_usb() {
    unsafe {
        if let Some(usb_dev) = USB_BUS.as_mut() {
            if let Some(serial) = USB_SERIAL.as_mut() {
                usb_dev.poll(&mut [serial]);
                let mut buf = [0u8; 32];
                if let Ok(count) = serial.read(&mut buf) {
                    let mut producer = RX_QUEUE.split().0;
                    for &b in &buf[..count] {
                        producer.enqueue(b).ok();
                    }
                }
            }
        }
    }
}

#[interrupt]
fn USB_OTHER() {
    poll_usb();
}
#[interrupt]
fn USB_TRCPT0() {
    poll_usb();
}
#[interrupt]
fn USB_TRCPT1() {
    poll_usb();
}

// ---------------- mascot state ----------------

#[derive(PartialEq, Clone, Copy)]
enum Mood {
    Sleeping,
    Idle,
    Thinking,
    Busy,
    Happy,
}

struct Mascot {
    mood: Mood,
    agents: u8,
    session_active: bool,
    muted: bool,
    happiness: i32, // 0..100
    frames_since_event: u32,
    mood_hold_frames: u32,
    happiness_tick: u32,
    banner: HString<20>,
    banner_frames: u32,
}

impl Mascot {
    fn new() -> Self {
        Self {
            mood: Mood::Idle,
            agents: 0,
            session_active: false,
            muted: false,
            happiness: 70,
            frames_since_event: 0,
            mood_hold_frames: 0,
            happiness_tick: 0,
            banner: HString::new(),
            banner_frames: 0,
        }
    }

    /// Returns a short beep "recipe" (freq_hz, times) to play, if any.
    fn handle_command(&mut self, cmd: &str) -> Option<[(u32, u32); 2]> {
        self.frames_since_event = 0;
        if cmd == "SESSION_START" {
            self.session_active = true;
            self.agents = 0;
            self.mood = Mood::Idle;
            return Some([(900, 50), (0, 0)]);
        } else if cmd == "SESSION_END" {
            self.session_active = false;
            self.agents = 0;
            self.mood = Mood::Sleeping;
        } else if cmd == "PROMPT" {
            self.mood = Mood::Thinking;
            self.mood_hold_frames = 120;
        } else if cmd == "AGENT_START" {
            self.agents = self.agents.saturating_add(1);
            self.mood = Mood::Busy;
            return Some([(1200, 35), (0, 0)]);
        } else if cmd == "AGENT_STOP" {
            self.agents = self.agents.saturating_sub(1);
            if self.agents == 0 {
                self.mood = if self.session_active {
                    Mood::Idle
                } else {
                    Mood::Sleeping
                };
            }
        } else if cmd == "STOP" {
            self.happiness = (self.happiness + 8).min(100);
            self.mood = Mood::Happy;
            self.mood_hold_frames = 50;
            return Some([(1500, 45), (1900, 45)]);
        } else if let Some(msg) = cmd.strip_prefix("NOTIFY:") {
            self.banner.clear();
            for ch in msg.chars().take(18) {
                let _ = self.banner.push(ch);
            }
            self.banner_frames = 80;
        }
        None
    }

    fn pet(&mut self) {
        self.happiness = (self.happiness + 15).min(100);
        self.mood = Mood::Happy;
        self.mood_hold_frames = 50;
    }

    fn update(&mut self) {
        self.frames_since_event = self.frames_since_event.saturating_add(1);
        if self.mood_hold_frames > 0 {
            self.mood_hold_frames -= 1;
        } else if self.mood == Mood::Thinking || self.mood == Mood::Happy {
            self.mood = if self.agents > 0 {
                Mood::Busy
            } else if self.session_active {
                Mood::Idle
            } else {
                Mood::Sleeping
            };
        }
        if self.agents > 0 {
            self.mood = Mood::Busy;
        }
        if !self.session_active && self.frames_since_event > 300 {
            self.mood = Mood::Sleeping; // ~15s at 50ms/frame
        }

        self.happiness_tick += 1;
        if self.happiness_tick > 600 {
            // ~30s at 50ms/frame
            self.happiness_tick = 0;
            self.happiness = (self.happiness - 1).max(0);
        }
        if self.banner_frames > 0 {
            self.banner_frames -= 1;
        }
    }
}

// ---------------- dot-matrix display ----------------
//
// The 320x240 panel is treated as a 64x48 grid of LED-style dots. Each dot is
// a DOT_PX x DOT_PX cell on the LCD of which only the top-left LIT_PX x LIT_PX
// block is lit, so the 1px gap stays black like on a real LED matrix. Scenes
// are composed into an off-screen `DotBuf` (using embedded-graphics, in dot
// coordinates) and `flush` then sends only the dots that changed since the
// previous frame, so nothing is cleared or redrawn wholesale.

const DOT_PX: usize = 8;
const LIT_PX: usize = 7;
const COLS: usize = 320 / DOT_PX; // 40 at 8px dots
const ROWS: usize = 240 / DOT_PX; // 30 at 8px dots
const _: () = assert!(LIT_PX <= DOT_PX);

// Layout, all in dots and derived from the grid size.
const FONT_W: i32 = 4;
const FONT_H: i32 = 6;
const HEADER_H: i32 = FONT_H;
const FOOTER_H: i32 = FONT_H + 1; // agent row / banner strip
const FOOTER_Y: i32 = ROWS as i32 - FOOTER_H;

const OFF: Rgb565 = Rgb565::BLACK;
const BODY: Rgb565 = Rgb565::new(27, 29, 10); // Clawd orange, ~(217,119,87)
const BODY_DIM: Rgb565 = Rgb565::new(18, 30, 13);
const TEXT: Rgb565 = Rgb565::new(30, 57, 27);
const HEART: Rgb565 = Rgb565::new(27, 17, 11);
const AGENT: Rgb565 = Rgb565::new(11, 42, 18);
const BANNER_BG: Rgb565 = Rgb565::new(11, 32, 15);

struct DotBuf {
    dots: [Rgb565; COLS * ROWS],
}

impl DotBuf {
    const fn new() -> Self {
        Self {
            dots: [OFF; COLS * ROWS],
        }
    }

    fn set(&mut self, x: i32, y: i32, c: Rgb565) {
        if x >= 0 && y >= 0 && (x as usize) < COLS && (y as usize) < ROWS {
            self.dots[y as usize * COLS + x as usize] = c;
        }
    }
}

impl OriginDimensions for DotBuf {
    fn size(&self) -> Size {
        Size::new(COLS as u32, ROWS as u32)
    }
}

impl DrawTarget for DotBuf {
    type Color = Rgb565;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Rgb565>>,
    {
        for Pixel(p, c) in pixels {
            self.set(p.x, p.y, c);
        }
        Ok(())
    }
}

/// Send only the dots that differ from what the LCD already shows.
fn flush<D>(lcd: &mut D, back: &DotBuf, front: &mut DotBuf) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    for i in 0..COLS * ROWS {
        if back.dots[i] != front.dots[i] {
            let x = (i % COLS) * DOT_PX;
            let y = (i / COLS) * DOT_PX;
            let area = Rectangle::new(
                Point::new(x as i32, y as i32),
                Size::new(LIT_PX as u32, LIT_PX as u32),
            );
            lcd.fill_solid(&area, back.dots[i])?;
            front.dots[i] = back.dots[i];
        }
    }
    Ok(())
}

// ---------------- drawing (all in dot coordinates) ----------------

fn compose(buf: &mut DotBuf, m: &Mascot, frame: u32) {
    buf.dots.fill(OFF);
    draw_header(buf, m);

    // bob amplitude is in sprite cells, so it scales with the sprite
    let bob = match m.mood {
        Mood::Busy => (libm::sinf(frame as f32 * 0.35) * 1.5) as i32,
        Mood::Happy => (libm::sinf(frame as f32 * 0.5) * 2.0) as i32,
        _ => (libm::sinf(frame as f32 * 0.08) * 1.5) as i32,
    } * SCALE
        / 2;
    draw_clawd(buf, m, frame, bob);

    if m.banner_frames > 0 {
        draw_banner(buf, m);
    } else {
        draw_agent_row(buf, m, frame);
    }
}

fn draw_header(buf: &mut DotBuf, m: &Mascot) {
    let style = MonoTextStyle::new(&FONT_4X6, TEXT);
    let mut s: HString<16> = HString::new();
    if !m.session_active {
        let _ = write!(s, "sleep");
    } else if m.agents > 0 {
        let _ = write!(s, "{}ag", m.agents);
    } else if m.mood == Mood::Thinking {
        let _ = write!(s, "think");
    } else {
        let _ = write!(s, "idle");
    }
    let _ = Text::with_baseline(&s, Point::new(1, 0), style, Baseline::Top).draw(buf);

    // happiness number, right-aligned just left of the heart
    let mut h: HString<4> = HString::new();
    let _ = write!(h, "{}", m.happiness);
    let heart_style = MonoTextStyle::new(&FONT_4X6, HEART);
    let x = COLS as i32 - HEART_W - 1 - h.len() as i32 * FONT_W;
    let _ = Text::with_baseline(&h, Point::new(x, 0), heart_style, Baseline::Top).draw(buf);
    draw_heart(buf, COLS as i32 - HEART_W, 0);
}

const HEART_W: i32 = 5;

fn draw_heart(buf: &mut DotBuf, x: i32, y: i32) {
    const ROWS5: [u8; 5] = [0b01010, 0b11111, 0b11111, 0b01110, 0b00100];
    for (r, bits) in ROWS5.iter().enumerate() {
        for c in 0..HEART_W {
            if bits & (0b10000 >> c) != 0 {
                buf.set(x + c, y + r as i32, HEART);
            }
        }
    }
}

// Clawd sprite, 24x15 cells (each drawn as SCALE x SCALE dots):
//   body   cols 3..=20, rows 0..=8
//   arms   cols 0..=2 and 21..=23, 3 rows tall (rows 6..=8, or 5..=7 when raised)
//   legs   4 legs 2 cells wide, rows 9..=14
//   eyes   1x3 slits carved out of the body at cols 6 and 17, rows 3..=5
const SPRITE_W: i32 = 24;
const SPRITE_H: i32 = 15;
// Dots per sprite cell: the largest integer scale that leaves a margin.
const SCALE: i32 = if COLS as i32 / (SPRITE_W + 8) > 0 {
    COLS as i32 / (SPRITE_W + 8)
} else {
    1
};
const SPRITE_PX_W: i32 = SPRITE_W * SCALE;
const SPRITE_PX_H: i32 = SPRITE_H * SCALE;

fn clawd_lit(x: i32, y: i32, arms_up: bool, legs_alt: bool) -> bool {
    let body = (3..=20).contains(&x) && (0..=8).contains(&y);
    let (a0, a1) = if arms_up { (5, 7) } else { (6, 8) };
    let arms = (a0..=a1).contains(&y) && (x <= 2 || x >= 21);
    let legs = (9..=14).contains(&y) && {
        let xs = if legs_alt {
            [3, 4, 6, 7, 16, 17, 19, 20]
        } else {
            [4, 5, 7, 8, 15, 16, 18, 19]
        };
        xs.contains(&x)
    };
    body || arms || legs
}

/// `eye` is three rows of a 3-bit pattern (MSB = left), centred on cols 6 and 17.
fn is_eye(x: i32, y: i32, eye: &[u8; 3]) -> bool {
    for ex in [5, 16] {
        if (ex..ex + 3).contains(&x) && (3..6).contains(&y) {
            return eye[(y - 3) as usize] & (0b100 >> (x - ex)) != 0;
        }
    }
    false
}

fn draw_clawd(buf: &mut DotBuf, m: &Mascot, frame: u32, bob: i32) {
    // centred horizontally, and vertically between header and footer
    let ox = (COLS as i32 - SPRITE_PX_W) / 2;
    let oy = HEADER_H + (FOOTER_Y - HEADER_H - SPRITE_PX_H) / 2 + bob;
    let body = if m.happiness < 25 { BODY_DIM } else { BODY };
    let step = m.mood == Mood::Busy && (frame / 3) % 2 == 1; // scurry while agents run
    let blink = frame % 100 < 3;

    let eye: [u8; 3] = if m.mood == Mood::Sleeping || blink {
        [0b000, 0b000, 0b111]
    } else if m.mood == Mood::Happy {
        [0b010, 0b101, 0b000]
    } else {
        [0b010, 0b010, 0b010]
    };
    let shift = if m.mood == Mood::Thinking {
        (libm::sinf(frame as f32 * 0.25) * 1.5) as i32
    } else {
        0
    };

    for y in 0..SPRITE_H {
        for x in 0..SPRITE_W {
            if !clawd_lit(x, y, step, step) {
                continue;
            }
            let c = if is_eye(x - shift, y, &eye) {
                OFF
            } else {
                body
            };
            for dy in 0..SCALE {
                for dx in 0..SCALE {
                    buf.set(ox + x * SCALE + dx, oy + y * SCALE + dy, c);
                }
            }
        }
    }

    if m.mood == Mood::Sleeping {
        let style = MonoTextStyle::new(&FONT_4X6, TEXT);
        let _ = Text::with_baseline("Zz", Point::new(ox + SPRITE_PX_W, oy), style, Baseline::Top)
            .draw(buf);
    }
    if m.mood == Mood::Thinking {
        for i in 0..(frame / 8) % 4 {
            let x = ox + SPRITE_PX_W + 3 * i as i32;
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                buf.set(x + dx, oy + 2 + dy, TEXT);
            }
        }
    }
}

fn draw_agent_row(buf: &mut DotBuf, m: &Mascot, frame: u32) {
    const SIZE: i32 = 4;
    const PITCH: i32 = SIZE + 1;
    // leave room for the "+N" overflow label
    const MAX_SHOWN: u8 = ((COLS as i32 - 2 * FONT_W) / PITCH) as u8;
    let shown = m.agents.min(MAX_SHOWN);
    let label_w = if m.agents > MAX_SHOWN { 2 * FONT_W } else { 0 };
    let start_x = (COLS as i32 - shown as i32 * PITCH - label_w) / 2;

    for i in 0..shown {
        let phase = frame as f32 * 0.1 + i as f32 * 0.8;
        let bounce = (libm::fabsf(libm::sinf(phase)) * 2.0) as i32;
        let (x, y) = (start_x + i as i32 * PITCH, FOOTER_Y + 2 - bounce);
        for dy in 0..SIZE {
            for dx in 0..SIZE {
                let corner = (dx == 0 || dx == SIZE - 1) && (dy == 0 || dy == SIZE - 1);
                if !corner {
                    buf.set(x + dx, y + dy, AGENT);
                }
            }
        }
    }
    if m.agents > MAX_SHOWN {
        let style = MonoTextStyle::new(&FONT_4X6, TEXT);
        let mut s: HString<8> = HString::new();
        let _ = write!(s, "+{}", m.agents - MAX_SHOWN);
        let _ = Text::with_baseline(
            &s,
            Point::new(start_x + shown as i32 * PITCH, FOOTER_Y + 1),
            style,
            Baseline::Top,
        )
        .draw(buf);
    }
}

fn draw_banner(buf: &mut DotBuf, m: &Mascot) {
    for y in FOOTER_Y..ROWS as i32 {
        for x in 1..COLS as i32 - 1 {
            buf.set(x, y, BANNER_BG);
        }
    }
    let mut s: HString<16> = HString::new();
    for ch in m.banner.chars().take((COLS as i32 / FONT_W - 1) as usize) {
        let _ = s.push(ch);
    }
    let style = MonoTextStyle::new(&FONT_4X6, Rgb565::WHITE);
    let _ = Text::with_baseline(&s, Point::new(2, FOOTER_Y + 1), style, Baseline::Top).draw(buf);
}

/// Bit-bang the piezo buzzer at `freq_hz` for `dur_ms`, using core-clock
/// cycle counting instead of a timer peripheral (see CPU_HZ note above).
fn beep(pin: &mut impl OutputPin, freq_hz: u32, dur_ms: u32) {
    if freq_hz == 0 {
        return;
    }
    let half_period_cycles = CPU_HZ / (freq_hz * 2);
    let total_cycles = (CPU_HZ / 1000) * dur_ms;
    let mut elapsed = 0u32;
    while elapsed < total_cycles {
        pin.set_high().ok();
        cortex_m::asm::delay(half_period_cycles);
        pin.set_low().ok();
        cortex_m::asm::delay(half_period_cycles);
        elapsed += half_period_cycles * 2;
    }
}

#[entry]
fn main() -> ! {
    let mut peripherals = Peripherals::take().unwrap();
    let mut core = CorePeripherals::take().unwrap();

    let mut clocks = GenericClockController::with_external_32kosc(
        peripherals.gclk,
        &mut peripherals.mclk,
        &mut peripherals.osc32kctrl,
        &mut peripherals.oscctrl,
        &mut peripherals.nvmctrl,
    );
    let mut delay = Delay::new(core.SYST, &mut clocks);
    let sets = wio::Pins::new(peripherals.port).split();

    let (mut display, _backlight) = sets
        .display
        .init(
            &mut clocks,
            peripherals.sercom7,
            &mut peripherals.mclk,
            58.MHz(),
            &mut delay,
        )
        .unwrap();

    // The BSP initializes the panel as LandscapeFlipped; Landscape is the 180° turn.
    display.set_orientation(Orientation::Landscape).ok();
    display.clear(Rgb565::BLACK).ok();

    let mut buzzer = sets.buzzer.ctr.into_push_pull_output();
    OutputPin::set_low(&mut buzzer).ok();

    // Plain polled GPIO for the three top buttons (bypasses the crate's
    // interrupt-driven ButtonController — simpler and plenty responsive at
    // our ~20fps loop rate).
    let btn_a = sets.buttons.button1.into_pull_up_input();
    let btn_b = sets.buttons.button2.into_pull_up_input();
    let btn_c = sets.buttons.button3.into_pull_up_input();

    // ---- USB CDC serial (interrupt-driven) ----
    let bus_allocator = unsafe {
        USB_ALLOCATOR = Some(sets.usb.usb_allocator(
            peripherals.usb,
            &mut clocks,
            &mut peripherals.mclk,
        ));
        USB_ALLOCATOR.as_ref().unwrap()
    };
    unsafe {
        USB_SERIAL = Some(SerialPort::new(bus_allocator));
        // Targets usb-device 0.3.x's builder (StringDescriptors replaced the
        // old .manufacturer()/.product() chain in 0.3 — see ../README.md if
        // this line doesn't match what Cargo actually resolved).
        USB_BUS = Some(
            UsbDeviceBuilder::new(bus_allocator, UsbVidPid(0x2886, 0x802d))
                .strings(&[StringDescriptors::new(LangID::EN)
                    .manufacturer("Claude Mascot")
                    .product("Claude Agent Mascot")
                    .serial_number("MASCOT1")])
                .unwrap()
                .device_class(USB_CLASS_CDC)
                .build(),
        );
    }
    unsafe {
        core.NVIC.set_priority(interrupt::USB_OTHER, 1);
        core.NVIC.set_priority(interrupt::USB_TRCPT0, 1);
        core.NVIC.set_priority(interrupt::USB_TRCPT1, 1);
        NVIC::unmask(interrupt::USB_OTHER);
        NVIC::unmask(interrupt::USB_TRCPT0);
        NVIC::unmask(interrupt::USB_TRCPT1);
    }

    let mut consumer = unsafe { RX_QUEUE.split().1 };
    let mut line: HString<64> = HString::new();
    let mut mascot = Mascot::new();
    let mut frame: u32 = 0;
    // `front` mirrors what the LCD shows (blank after the clear above); each
    // frame is composed into `back` and only the differing dots are sent.
    let mut back = DotBuf::new();
    let mut front = DotBuf::new();
    let (mut last_a, mut last_b, mut last_c) = (true, true, true);

    loop {
        while let Some(byte) = consumer.dequeue() {
            if byte == b'\n' {
                let cmd = line.trim();
                if let Some(notes) = mascot.handle_command(cmd) {
                    if !mascot.muted {
                        for (freq, dur) in notes {
                            beep(&mut buzzer, freq, dur);
                        }
                    }
                }
                line.clear();
            } else if byte != b'\r' {
                if line.push(byte as char).is_err() {
                    line.clear();
                }
            }
        }

        let a = btn_a.is_low().unwrap_or(false);
        let b = btn_b.is_low().unwrap_or(false);
        let c = btn_c.is_low().unwrap_or(false);
        if a && !last_a {
            mascot.muted = !mascot.muted;
        }
        if c && !last_c {
            mascot.pet();
            if !mascot.muted {
                beep(&mut buzzer, 1400, 60);
            }
        }
        // (B is reserved for a debug overlay in the C++ version; omitted
        // here for brevity — add a `show_debug` flag the same way if wanted.)
        let _ = b;
        last_a = a;
        last_b = b;
        last_c = c;

        mascot.update();
        compose(&mut back, &mascot, frame);
        flush(&mut display, &back, &mut front).ok();

        delay.delay_ms(50u32);
        frame = frame.wrapping_add(1);
    }
}
