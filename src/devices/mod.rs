//! Device drivers. Each driver runs on its own thread, pushes samples straight
//! into the shared [`Store`](crate::model::Store) and asks egui to repaint, so
//! a new reading is on screen (and in the overlay) within one frame.

pub mod owon;
pub mod owon_spm;
pub mod powermon;
pub mod sim;

use std::io::{self, Read, Write};
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
    /// OWON SPM3051/6053/3103/6103: programmable supply with built-in
    /// multimeter, SCPI over USB serial.
    OwonSpm,
    /// The real SPM driver talking to a simulated SPM.
    SpmSimulator,
    Simulator,
}

impl PowerSourceKind {
    pub const ALL: [PowerSourceKind; 4] = [
        PowerSourceKind::PowerMon,
        PowerSourceKind::OwonSpm,
        PowerSourceKind::SpmSimulator,
        PowerSourceKind::Simulator,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PowerSourceKind::PowerMon => "PowerMon (INA228, USB)",
            PowerSourceKind::OwonSpm => "OWON SPM (USB)",
            PowerSourceKind::SpmSimulator => "OWON SPM (Simulator)",
            PowerSourceKind::Simulator => "Simulator",
        }
    }

    /// The supply itself can be read and controlled (set points, output).
    pub fn is_spm(self) -> bool {
        matches!(self, PowerSourceKind::OwonSpm | PowerSourceKind::SpmSimulator)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DmmKind {
    /// OWON XDM1041 / XDM1241 / XDM2041 over USB serial (SCPI).
    #[default]
    OwonXdm,
    /// The multimeter built into an OWON SPM supply; runs on the supply's
    /// own connection.
    OwonSpm,
    Simulator,
}

impl DmmKind {
    pub const ALL: [DmmKind; 3] = [DmmKind::OwonXdm, DmmKind::OwonSpm, DmmKind::Simulator];

    pub fn label(self) -> &'static str {
        match self {
            DmmKind::OwonXdm => "OWON XDM1241 (SCPI)",
            DmmKind::OwonSpm => "Im Netzteil (OWON SPM)",
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

/// Commands the UI can send to a controllable supply. The driver clamps
/// every value to the model's limits before sending it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PsuCommand {
    SetVoltage(f64),
    SetCurrent(f64),
    Output(bool),
    /// Over-voltage protection threshold.
    SetOvp(f64),
    /// Over-current protection threshold.
    SetOcp(f64),
}

/// A running driver thread. Dropping it stops the thread.
pub struct DeviceHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    pub commands: Option<Sender<DmmCommand>>,
    pub psu: Option<Sender<PsuCommand>>,
}

impl DeviceHandle {
    pub fn spawn(name: &str, f: impl FnOnce(Arc<AtomicBool>) + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let join = std::thread::Builder::new().name(name.into()).spawn(move || f(s)).expect("spawn device thread");
        Self { stop, join: Some(join), commands: None, psu: None }
    }

    pub fn is_finished(&self) -> bool {
        self.join.as_ref().is_none_or(|j| j.is_finished())
    }

    pub fn send(&self, cmd: DmmCommand) {
        if let Some(tx) = &self.commands {
            let _ = tx.send(cmd);
        }
    }

    /// Returns false when this driver can't control a supply.
    pub fn send_psu(&self, cmd: PsuCommand) -> bool {
        self.psu.as_ref().is_some_and(|tx| tx.send(cmd).is_ok())
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

/// Anything a [`ScpiLink`] can talk over: a real serial port or the
/// simulated SPM. `clear_input` throws away what has arrived but wasn't read.
pub trait ScpiPort: Read + Write + Send {
    fn clear_input(&mut self);
}

impl ScpiPort for Box<dyn serialport::SerialPort> {
    fn clear_input(&mut self) {
        let _ = (**self).clear(serialport::ClearBuffer::Input);
    }
}

/// How long a set command may take to answer `ERR` (no answer = accepted).
const ERR_WINDOW: Duration = Duration::from_millis(150);
/// After a retried query: quiet time that means no second answer follows
/// (about two round trips).
const RETRY_SETTLE: Duration = Duration::from_millis(40);

/// Line-based SCPI transactions with the pacing OWON instruments need: at
/// least `gap` between two transactions (flooding locks the SPM up for
/// seconds), and one retry when a query answer goes missing.
pub struct ScpiLink<P: ScpiPort> {
    port: P,
    buf: Vec<u8>,
    /// Last write or last received line.
    last_io: Instant,
    pub gap: Duration,
    /// Wait per query attempt.
    pub timeout: Duration,
}

impl<P: ScpiPort> ScpiLink<P> {
    pub fn new(port: P) -> Self {
        Self {
            port,
            buf: Vec::new(),
            last_io: Instant::now() - Duration::from_secs(1),
            gap: Duration::from_millis(5),
            timeout: Duration::from_millis(300),
        }
    }

    #[cfg(test)]
    pub fn port_mut(&mut self) -> &mut P {
        &mut self.port
    }

    /// Sends one command (no `;` chains: the SPM only runs the first one).
    pub fn send(&mut self, cmd: &str) -> io::Result<()> {
        let since = self.last_io.elapsed();
        if since < self.gap {
            std::thread::sleep(self.gap - since);
        }
        let mut line = Vec::with_capacity(cmd.len() + 1);
        line.extend_from_slice(cmd.as_bytes());
        line.push(b'\n');
        self.port.write_all(&line)?;
        self.port.flush()?;
        self.last_io = Instant::now();
        Ok(())
    }

    /// Next non-empty line, or `None` after `timeout`.
    fn read_reply(&mut self, timeout: Duration) -> io::Result<Option<String>> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match read_line(&mut self.port, &mut self.buf, left)? {
                Some(l) if l.is_empty() => continue,
                Some(l) => {
                    self.last_io = Instant::now();
                    return Ok(Some(l));
                }
                None => return Ok(None),
            }
        }
    }

    /// Sends a query and returns the answer line. A query right after a
    /// state change is sometimes dropped, so a missing answer is asked for
    /// once more. Leftovers are thrown away before each try, and after a
    /// retry the line is read until it goes quiet: the first try may still
    /// answer late, and that extra line must not be taken for the answer to
    /// the next query. Callers should still check the shape of the answer.
    pub fn query(&mut self, cmd: &str) -> io::Result<String> {
        for attempt in 0..2 {
            self.clear();
            self.send(cmd)?;
            if let Some(mut line) = self.read_reply(self.timeout)? {
                if attempt > 0 {
                    // Both answers belong to this query; keep the newest.
                    while let Some(later) = self.read_reply(RETRY_SETTLE)? {
                        line = later;
                    }
                    self.clear();
                }
                return Ok(line);
            }
        }
        Err(io::Error::new(io::ErrorKind::TimedOut, format!("keine Antwort auf {cmd}")))
    }

    /// Sends a set command. OWON answers nothing on success and `ERR` on
    /// failure, so wait a moment for an `ERR`. Returns false if it came.
    pub fn send_checked(&mut self, cmd: &str) -> io::Result<bool> {
        self.send(cmd)?;
        let deadline = Instant::now() + ERR_WINDOW;
        while let Some(line) = self.read_reply(deadline.saturating_duration_since(Instant::now()))? {
            if line.eq_ignore_ascii_case("ERR") {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Forgets everything received but not read yet.
    pub fn clear(&mut self) {
        self.port.clear_input();
        self.buf.clear();
    }

    /// Waits for stragglers, then throws them away (e.g. after a timeout).
    pub fn drain(&mut self) {
        std::thread::sleep(Duration::from_millis(100));
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Answers every query with `reply`, after dropping the first `drop` of
    /// them, and records when each command arrived.
    struct ScriptPort {
        reply: &'static str,
        drop: usize,
        out: VecDeque<u8>,
        writes: Vec<(Instant, String)>,
        cleared: usize,
    }

    impl ScriptPort {
        fn new(reply: &'static str, drop: usize) -> Self {
            Self { reply, drop, out: VecDeque::new(), writes: Vec::new(), cleared: 0 }
        }
    }

    impl Read for ScriptPort {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.out.is_empty() {
                std::thread::sleep(Duration::from_millis(1));
                return Err(io::ErrorKind::TimedOut.into());
            }
            let n = buf.len().min(self.out.len());
            for b in buf.iter_mut().take(n) {
                *b = self.out.pop_front().unwrap_or_default();
            }
            Ok(n)
        }
    }

    impl Write for ScriptPort {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let cmd = String::from_utf8_lossy(buf).trim().to_string();
            self.writes.push((Instant::now(), cmd.clone()));
            if cmd.ends_with('?') {
                if self.drop > 0 {
                    self.drop -= 1;
                } else {
                    self.out.extend(format!("{}\r\n", self.reply).bytes());
                }
            } else if cmd.starts_with("BAD") {
                self.out.extend(b"ERR\r\n");
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl ScpiPort for ScriptPort {
        fn clear_input(&mut self) {
            self.cleared += 1;
            self.out.clear();
        }
    }

    #[test]
    fn scpi_link_keeps_gap_between_transactions() {
        let mut link = ScpiLink::new(ScriptPort::new("5.000", 0));
        for _ in 0..5 {
            assert_eq!(link.query("VOLT?").unwrap(), "5.000");
        }
        link.send("OUTP OFF").unwrap();
        let w = &link.port_mut().writes;
        assert_eq!(w.len(), 6);
        assert!(w.windows(2).all(|p| p[1].0 - p[0].0 >= Duration::from_millis(5)));
    }

    #[test]
    fn scpi_link_retries_a_dropped_answer_once() {
        let mut link = ScpiLink::new(ScriptPort::new("1.000", 1));
        link.timeout = Duration::from_millis(30);
        assert_eq!(link.query("CURR?").unwrap(), "1.000");
        assert_eq!(link.port_mut().writes.len(), 2);
        // before each try, and once more after the retry settled
        assert_eq!(link.port_mut().cleared, 3);

        let mut link = ScpiLink::new(ScriptPort::new("1.000", 2));
        link.timeout = Duration::from_millis(30);
        let e = link.query("CURR?").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn scpi_link_detects_err_on_set() {
        let mut link = ScpiLink::new(ScriptPort::new("", 0));
        assert!(link.send_checked("VOLT 5.000").unwrap());
        assert!(!link.send_checked("BAD 1").unwrap());
    }

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
