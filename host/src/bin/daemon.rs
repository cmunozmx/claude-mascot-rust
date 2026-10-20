//! Claude Mascot daemon (Rust port of the Python version).
//!
//! Listens on udp://127.0.0.1:8765 for short text events sent by
//! `mascot-hook`, and forwards each one as a line to the Wio Terminal over
//! USB serial. UDP means a hook never blocks or errors just because this
//! daemon (or the board) isn't running.
//!
//! Usage:
//!   cargo run --release --bin mascot-daemon             # auto-detect port
//!   cargo run --release --bin mascot-daemon /dev/tty...  # explicit port

use std::env;
use std::io::Write;
use std::net::UdpSocket;
use std::thread::sleep;
use std::time::Duration;

const UDP_ADDR: &str = "127.0.0.1:8765";
const BAUD: u32 = 115_200;
const NAME_HINTS: [&str; 5] = ["wio", "seeed", "samd", "usbmodem", "acm"];

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
        match serialport::new(port, BAUD).timeout(Duration::from_secs(1)).open() {
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

fn main() {
    let port = match env::args().nth(1).or_else(find_port) {
        Some(p) => p,
        None => {
            eprintln!("No serial port found/specified. Plug in the Wio Terminal and try again.");
            std::process::exit(1);
        }
    };

    let mut ser = connect(&port);

    let sock = UdpSocket::bind(UDP_ADDR).expect("failed to bind UDP socket (is another daemon already running?)");
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

        let line = format!("{text}\n");
        if let Err(e) = ser.write_all(line.as_bytes()) {
            eprintln!("[mascot] serial write failed ({e}), reconnecting...");
            ser = connect(&port);
            let _ = ser.write_all(line.as_bytes());
        }
    }
}
