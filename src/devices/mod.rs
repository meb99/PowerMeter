//! Device drivers. Each driver runs on its own thread, pushes samples straight
//! into the shared [`Store`](crate::model::Store) and asks egui to repaint, so
//! a new reading is on screen (and in the overlay) within one frame.

pub mod owon;
pub mod powermon;
pub mod sim;

use std::io::{self, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::model::DmmFunction;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PowerSourceKind {
    /// ESP32/RP2040 + INA228 in-line meter (see `firmware/powermon`).
    #[default]
    PowerMon,
    Simulator,
}

impl PowerSourceKind {
    pub fn label(self) -> &'static str {
        match self {
            PowerSourceKind::PowerMon => "PowerMon (INA228, USB)",
            PowerSourceKind::Simulator => "Simulator",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DmmKind {
    /// OWON XDM1041 / XDM1241 / XDM2041 over USB serial (SCPI).
    #[default]
    OwonXdm,
    Simulator,
}

impl DmmKind {
    pub fn label(self) -> &'static str {
        match self {
            DmmKind::OwonXdm => "OWON XDM1241 (SCPI)",
            DmmKind::Simulator => "Simulator",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DmmRate {
    #[default]
    Fast,
    Medium,
    Slow,
}

impl DmmRate {
    pub fn label(self) -> &'static str {
        match self {
            DmmRate::Fast => "Schnell",
            DmmRate::Medium => "Mittel",
            DmmRate::Slow => "Langsam (genauer)",
        }
    }

    pub fn scpi(self) -> &'static str {
        match self {
            DmmRate::Fast => "RATE F",
            DmmRate::Medium => "RATE M",
            DmmRate::Slow => "RATE L",
        }
    }
}

/// Commands the UI can send to a running multimeter driver.
#[derive(Clone, Debug)]
pub enum DmmCommand {
    SetFunction(DmmFunction),
    SetRate(DmmRate),
}

/// A running driver thread. Dropping it stops the thread.
pub struct DeviceHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    pub commands: Option<Sender<DmmCommand>>,
}

impl DeviceHandle {
    pub fn spawn(name: &str, f: impl FnOnce(Arc<AtomicBool>) + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let join = std::thread::Builder::new().name(name.into()).spawn(move || f(s)).expect("spawn device thread");
        Self { stop, join: Some(join), commands: None }
    }

    pub fn is_finished(&self) -> bool {
        self.join.as_ref().is_none_or(|j| j.is_finished())
    }

    pub fn send(&self, cmd: DmmCommand) {
        if let Some(tx) = &self.commands {
            let _ = tx.send(cmd);
        }
    }
}

impl Drop for DeviceHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

pub fn stopped(stop: &AtomicBool) -> bool {
    stop.load(Ordering::Relaxed)
}

/// Serial ports worth offering. On macOS every device shows up twice
/// (`/dev/tty.*` and `/dev/cu.*`); the `cu` node is the one to open.
pub fn list_ports() -> Vec<String> {
    let mut ports: Vec<String> = serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.port_name)
        .filter(|n| !n.contains("Bluetooth") && !n.contains("debug-console"))
        .filter(|n| !(cfg!(target_os = "macos") && n.starts_with("/dev/tty.")))
        .collect();
    ports.sort();
    ports.dedup();
    ports
}

/// Reads one `\n`-terminated line with an overall deadline. Returns
/// `Ok(None)` on timeout. `buf` keeps partial data between calls.
pub fn read_line(port: &mut dyn Read, buf: &mut Vec<u8>, timeout: Duration) -> io::Result<Option<String>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let s = String::from_utf8_lossy(&line).trim().to_string();
            return Ok(Some(s));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        let mut chunk = [0u8; 256];
        match port.read(&mut chunk) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Gerät getrennt")),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::TimedOut || e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
        if buf.len() > 64 * 1024 {
            buf.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_lines_across_chunks() {
        let data = b"1.0\r\n2.5\npartial";
        let mut cursor = io::Cursor::new(&data[..]);
        let mut buf = Vec::new();
        let t = Duration::from_millis(10);
        assert_eq!(read_line(&mut cursor, &mut buf, t).unwrap().as_deref(), Some("1.0"));
        assert_eq!(read_line(&mut cursor, &mut buf, t).unwrap().as_deref(), Some("2.5"));
        assert!(read_line(&mut cursor, &mut buf, t).is_err()); // EOF
        assert_eq!(buf, b"partial");
    }
}
