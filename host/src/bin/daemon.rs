//! Claude Mascot daemon (Rust port of the Python version).
//!
//! Listens on udp://127.0.0.1:8765 for short text events sent by
//! `mascot-hook`, and forwards each one as a line to the Wio Terminal over
//! USB serial. UDP means a hook never blocks or errors just because this
//! daemon (or the board) isn't running.
//!
//! If herdr is running, the daemon also subscribes to its event socket and
//! sends the state of every agent as one `AGENTS:<states>` line (one char per
//! agent: B blocked, W working, I idle/done, U unknown). herdr is optional:
//! while it isn't reachable the daemon just retries quietly.
//!
//! Usage:
//!   cargo run --release --bin mascot-daemon             # auto-detect port
//!   cargo run --release --bin mascot-daemon /dev/tty...  # explicit port
//!   cargo run --release --bin mascot-daemon -- --herdr-dry-run
//!       # print the AGENTS lines instead of sending them; touches neither
//!       # the serial port nor the UDP socket

use serde_json::{json, Value};
use std::env;
use std::io::{self, BufRead, BufReader, Write};
use std::net::UdpSocket;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::{self, sleep};
use std::time::Duration;

const UDP_ADDR: &str = "127.0.0.1:8765";
const BAUD: u32 = 115_200;
const NAME_HINTS: [&str; 5] = ["wio", "seeed", "samd", "usbmodem", "acm"];
const MAX_AGENTS: usize = 32;

fn find_port() -> Option<String> {
    let ports = serialport::available_ports().unwrap_or_default();
    for p in &ports {
        let desc = format!("{:?}", p.port_type).to_lowercase();
        if NAME_HINTS.iter().any(|h| desc.contains(h)) {
            return Some(p.port_name.clone());
        }
    }
    if ports.len() == 1 {
        return Some(ports[0].port_name.clone());
    }
    if !ports.is_empty() {
        eprintln!("Multiple serial ports found, none obviously the Wio Terminal:");
        for p in &ports {
            eprintln!("  {}", p.port_name);
        }
        eprintln!("Re-run with the port as an argument.");
    }
    None
}

fn connect(port: &str) -> Box<dyn serialport::SerialPort> {
    loop {
        match serialport::new(port, BAUD)
            .timeout(Duration::from_secs(1))
            .open()
        {
            Ok(ser) => {
                sleep(Duration::from_millis(1500)); // let the board settle
                println!("[mascot] connected to {port}");
                return ser;
            }
            Err(e) => {
                eprintln!("[mascot] couldn't open {port} ({e}), retrying in 3s...");
                sleep(Duration::from_secs(3));
            }
        }
    }
}

// ---------------- serial port shared by the UDP loop and the herdr thread ----------------

struct Board {
    port: String,
    ser: Box<dyn serialport::SerialPort>,
    /// Latest AGENTS state, replayed after a serial reconnect (the board
    /// forgets it when it resets).
    agents: Option<String>,
}

impl Board {
    fn write_line(&mut self, line: &str) {
        let data = format!("{line}\n");
        if let Err(e) = self.ser.write_all(data.as_bytes()) {
            eprintln!("[mascot] serial write failed ({e}), reconnecting...");
            self.ser = connect(&self.port);
            if let Some(a) = &self.agents {
                let _ = self.ser.write_all(format!("AGENTS:{a}\n").as_bytes());
            }
            let _ = self.ser.write_all(data.as_bytes());
        }
    }

    fn set_agents(&mut self, states: &str) {
        self.agents = Some(states.to_string());
        self.write_line(&format!("AGENTS:{states}"));
    }
}

fn lock(board: &Mutex<Board>) -> std::sync::MutexGuard<'_, Board> {
    board.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------- herdr: agent states over its event socket ----------------
//
// Framing (newline-delimited JSON over a Unix socket), read-only calls only:
//   -> {"id":"1","method":"agent.list","params":{}}
//   <- {"id":"1","result":{"type":"agent_list","agents":[{"pane_id":"w1:p1","agent_status":"blocked",..},..]}}
//   -> {"id":"2","method":"events.subscribe","params":{"subscriptions":[
//         {"type":"pane.created"},{"type":"pane.closed"},{"type":"pane.exited"},
//         {"type":"pane.agent_detected"},
//         {"type":"pane.agent_status_changed","pane_id":"w1:p1"}, ...]}}
//   <- {"id":"2","result":{"type":"subscription_started"}}   then events, one per line.
// Status subscriptions are per pane, so whenever the set of panes/agents
// changes (created/closed/exited/agent_detected) the session is restarted
// from a fresh list.

fn herdr_socket() -> Option<PathBuf> {
    if let Some(p) = env::var_os("HERDR_SOCKET_PATH") {
        return Some(PathBuf::from(p));
    }
    env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/herdr/herdr.sock"))
}

fn status_code(status: &str) -> char {
    match status {
        "blocked" => 'B',
        "working" => 'W',
        "idle" | "done" => 'I',
        _ => 'U',
    }
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn read_json(reader: &mut impl BufRead) -> io::Result<Value> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "herdr closed the connection",
        ));
    }
    serde_json::from_str(&line).map_err(|e| bad(format!("bad JSON from herdr: {e}")))
}

fn request(
    stream: &mut UnixStream,
    reader: &mut impl BufRead,
    id: &str,
    method: &str,
    params: Value,
) -> io::Result<Value> {
    let req = json!({ "id": id, "method": method, "params": params });
    stream.write_all(format!("{req}\n").as_bytes())?;
    let resp = read_json(reader)?;
    if let Some(err) = resp.get("error") {
        return Err(bad(format!("herdr {method} failed: {err}")));
    }
    Ok(resp)
}

fn list_agents(path: &Path) -> io::Result<Vec<(String, char)>> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let resp = request(&mut stream, &mut reader, "list", "agent.list", json!({}))?;
    let agents = resp["result"]["agents"]
        .as_array()
        .ok_or_else(|| bad("agent.list: no agents array"))?;
    Ok(agents
        .iter()
        .filter_map(|a| {
            Some((
                a["pane_id"].as_str()?.to_string(),
                status_code(a["agent_status"].as_str().unwrap_or("")),
            ))
        })
        .collect())
}

#[derive(PartialEq, Clone, Copy)]
enum Link {
    Unknown,
    Up,
    Down,
}

struct Herdr {
    path: PathBuf,
    /// What was last logged, so connects and drops are reported once each.
    link: Link,
    /// (pane_id, status) in order of first sight; only ever appended to or
    /// pruned, so dots don't jump around when statuses change.
    order: Vec<(String, char)>,
    last: Option<String>,
    emit: Box<dyn FnMut(&str) + Send>,
}

impl Herdr {
    fn new(path: PathBuf, emit: Box<dyn FnMut(&str) + Send>) -> Self {
        Self {
            path,
            link: Link::Unknown,
            order: Vec::new(),
            last: None,
            emit,
        }
    }

    /// Call `emit` with the current states string, but only if it changed.
    fn publish(&mut self) {
        let states: String = self
            .order
            .iter()
            .take(MAX_AGENTS)
            .map(|(_, c)| *c)
            .collect();
        if self.last.as_deref() != Some(states.as_str()) {
            (self.emit)(&states);
            self.last = Some(states);
        }
    }

    fn resync(&mut self, agents: Vec<(String, char)>) {
        self.order
            .retain(|(id, _)| agents.iter().any(|(a, _)| a == id));
        for (id, st) in agents {
            match self.order.iter_mut().find(|(i, _)| *i == id) {
                Some(slot) => slot.1 = st,
                None => self.order.push((id, st)),
            }
        }
    }

    /// One subscription session. Ok(()) means "the set of agents changed,
    /// restart me"; Err means the connection is gone.
    fn session(&mut self, connected: &mut bool) -> io::Result<()> {
        let initial = list_agents(&self.path)?;
        let mut subs = vec![
            json!({"type": "pane.created"}),
            json!({"type": "pane.closed"}),
            json!({"type": "pane.exited"}),
            json!({"type": "pane.agent_detected"}),
        ];
        subs.extend(
            initial
                .iter()
                .map(|(id, _)| json!({"type": "pane.agent_status_changed", "pane_id": id})),
        );
        self.resync(initial);

        let mut stream = UnixStream::connect(&self.path)?;
        let mut reader = BufReader::new(stream.try_clone()?);
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let resp = request(
            &mut stream,
            &mut reader,
            "sub",
            "events.subscribe",
            json!({ "subscriptions": subs }),
        )?;
        if resp["result"]["type"] != "subscription_started" {
            return Err(bad(format!("unexpected subscribe response: {resp}")));
        }
        stream.set_read_timeout(None)?;
        *connected = true;
        if self.link != Link::Up {
            println!("[mascot] herdr connected ({})", self.path.display());
            self.link = Link::Up;
        }

        // A status may have changed between the list and the subscription.
        let fresh = list_agents(&self.path)?;
        self.resync(fresh);
        self.publish();

        loop {
            let ev = read_json(&mut reader)?;
            let data = ev.get("data").unwrap_or(&ev);
            let kind = ev["event"]
                .as_str()
                .or_else(|| ev["type"].as_str())
                .or_else(|| data["type"].as_str())
                .unwrap_or("");
            if kind.contains("agent_status_changed") {
                if let (Some(id), Some(st)) =
                    (data["pane_id"].as_str(), data["agent_status"].as_str())
                {
                    if let Some(slot) = self.order.iter_mut().find(|(i, _)| i == id) {
                        slot.1 = status_code(st);
                        self.publish();
                    }
                }
            } else if [
                "pane_created",
                "pane_closed",
                "pane_exited",
                "pane_agent_detected",
            ]
            .iter()
            .any(|k| kind.replace('.', "_") == *k)
            {
                return Ok(());
            }
        }
    }

    fn run(&mut self) -> ! {
        let mut backoff = 1u64;
        loop {
            let mut connected = false;
            let result = self.session(&mut connected);
            if connected {
                backoff = 1;
            }
            match result {
                Ok(()) => sleep(Duration::from_millis(200)), // pane set changed: resync right away
                Err(e) => {
                    if self.link != Link::Down {
                        if self.link == Link::Up {
                            // herdr went away, and its agents with it
                            self.order.clear();
                            self.publish();
                            eprintln!("[mascot] herdr connection lost ({e}), retrying quietly...");
                        } else {
                            eprintln!("[mascot] herdr unavailable ({e}), retrying quietly...");
                        }
                        self.link = Link::Down;
                    }
                    sleep(Duration::from_secs(backoff));
                    backoff = (backoff * 2).min(10);
                }
            }
        }
    }
}

fn main() {
    let mut dry_run = false;
    let mut port_arg = None;
    for arg in env::args().skip(1) {
        if arg == "--herdr-dry-run" {
            dry_run = true;
        } else if port_arg.is_none() {
            port_arg = Some(arg);
        }
    }
    let herdr_path = herdr_socket();

    if dry_run {
        let Some(path) = herdr_path else {
            eprintln!("Can't locate the herdr socket (set HERDR_SOCKET_PATH or HOME).");
            std::process::exit(1);
        };
        println!(
            "[mascot] herdr dry run on {} (serial and UDP untouched)",
            path.display()
        );
        Herdr::new(
            path,
            Box::new(|s| {
                println!("AGENTS:{s}");
                let _ = io::stdout().flush();
            }),
        )
        .run();
    }

    let port = match port_arg.or_else(find_port) {
        Some(p) => p,
        None => {
            eprintln!("No serial port found/specified. Plug in the Wio Terminal and try again.");
            std::process::exit(1);
        }
    };

    let ser = connect(&port);
    let board = Arc::new(Mutex::new(Board {
        port,
        ser,
        agents: None,
    }));

    if let Some(path) = herdr_path {
        let board = Arc::clone(&board);
        thread::spawn(move || {
            Herdr::new(path, Box::new(move |s| lock(&board).set_agents(s))).run();
        });
    }

    let sock = UdpSocket::bind(UDP_ADDR)
        .expect("failed to bind UDP socket (is another daemon already running?)");
    println!("[mascot] listening for events on udp://{UDP_ADDR}");
    println!("[mascot] ready. Ctrl+C to stop.");

    let mut buf = [0u8; 1024];
    loop {
        let (n, _addr) = match sock.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[mascot] udp recv error: {e}");
                continue;
            }
        };
        let text = String::from_utf8_lossy(&buf[..n]).trim().to_string();
        if text.is_empty() {
            continue;
        }
        println!("[mascot] event: {text}");
        lock(&board).write_line(&text);
    }
}
