//! OWON XDM1041 / XDM1241 / XDM2041 bench multimeter over USB serial (SCPI).
//!
//! The meter answers `MEAS?` (some firmware only `MEAS1?`) with the value on
//! the main display in plain scientific notation (`8.924057E-01`). We poll
//! every [`POLL_PERIOD`] and check `FUNC?` once a second so a key pressed on
//! the meter would be picked up.
//!
//! Remote mode: a real XDM1241 (Windows, 2026-11) answered `*IDN?` but stayed
//! silent on `FUNC?` unless `SYST:REM` came first, so we always send
//! `SYST:REM` right after `*IDN?`, as the first version did. That locks the
//! meter's keys while connected; the function is switched in the app, and
//! `SYST:LOC` gives the keys back when disconnecting. A meter that never
//! answers `FUNC?` still works: the function then follows the app's buttons.
//! XDM1041/XDM1241 firmware before V4.3.0 swaps DIOD and CONT in the `FUNC?`
//! reply (setting them with `CONF:` works); [`swaps_diod_cont`] undoes that.

use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui;

use super::{DmmCommand, DmmRate, read_line, stopped};
use crate::model::{ConnState, DmmFunction, Shared};

/// Shortest time between two readings (at most 50 per second, as
/// rusty_meter polls), so the meter is never queried back to back.
pub const POLL_PERIOD: Duration = Duration::from_millis(20);

/// How long the meter may take to answer a query.
const ANSWER_TIMEOUT: Duration = Duration::from_millis(1500);

pub struct OwonConfig {
    pub port: String,
    pub baud: u32,
    pub rate: DmmRate,
}

pub fn run(cfg: OwonConfig, store: Shared, ctx: egui::Context, rx: Receiver<DmmCommand>, stop: Arc<AtomicBool>) {
    let set_conn = |c: ConnState| {
        store.lock().unwrap().dmm.conn = c;
        ctx.request_repaint();
    };
    set_conn(ConnState::Connecting);

    // Reconnect loop: unplugging the meter mid-video shouldn't need a click.
    while !stopped(&stop) {
        let result =
            open_serial(&cfg.port, cfg.baud).and_then(|port| session(port, cfg.rate, &store, &ctx, &rx, &stop));
        match result {
            Ok(()) => break,
            Err(e) => {
                set_conn(ConnState::Error(format!("{e} – neuer Versuch …")));
                let until = Instant::now() + Duration::from_secs(2);
                while Instant::now() < until && !stopped(&stop) {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
    set_conn(ConnState::Disconnected);
}

fn open_serial(port: &str, baud: u32) -> Result<Box<dyn serialport::SerialPort>, String> {
    serialport::new(port, baud).timeout(Duration::from_millis(20)).open().map_err(|e| format!("{port}: {e}"))
}

struct Link {
    port: Box<dyn serialport::SerialPort>,
    buf: Vec<u8>,
}

impl Link {
    fn send(&mut self, cmd: &str) -> io::Result<()> {
        self.port.write_all(cmd.as_bytes())?;
        self.port.write_all(b"\n")?;
        self.port.flush()
    }

    /// The answer, or `None` when the meter stays silent.
    fn try_query(&mut self, cmd: &str) -> io::Result<Option<String>> {
        self.send(cmd)?;
        read_line(&mut self.port, &mut self.buf, ANSWER_TIMEOUT)
    }

    fn query(&mut self, cmd: &str) -> io::Result<String> {
        self.try_query(cmd)?.ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, format!("keine Antwort auf {cmd}")))
    }

    /// Throws away anything still in flight (e.g. after a mode change).
    fn drain(&mut self) {
        std::thread::sleep(Duration::from_millis(150));
        let _ = self.port.clear(serialport::ClearBuffer::Input);
        self.buf.clear();
    }
}

fn session(
    port: Box<dyn serialport::SerialPort>,
    rate: DmmRate,
    store: &Shared,
    ctx: &egui::Context,
    rx: &Receiver<DmmCommand>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let mut link = Link { port, buf: Vec::new() };
    link.drain();

    let idn = link.query("*IDN?").map_err(|e| e.to_string())?;
    let swap_diod_cont = swaps_diod_cont(&idn);
    link.send("SYST:REM").map_err(|e| e.to_string())?;
    link.send(rate.scpi()).map_err(|e| e.to_string())?;
    link.drain();

    let read_func = |answer: &str| match DmmFunction::from_scpi(answer) {
        DmmFunction::Cont if swap_diod_cont => DmmFunction::Diode,
        DmmFunction::Diode if swap_diod_cont => DmmFunction::Cont,
        f => f,
    };
    // FUNC? is a nice-to-have: without it the function follows the app.
    let func = link.try_query("FUNC?").map_err(|e| e.to_string())?;
    let func_supported = func.is_some();
    // MEAS? on most firmware, MEAS1? on some.
    let mut meas_cmd = "MEAS?";
    let mut first = link.try_query(meas_cmd).map_err(|e| e.to_string())?;
    if first.is_none() {
        meas_cmd = "MEAS1?";
        first = link.try_query(meas_cmd).map_err(|e| e.to_string())?;
    }
    if first.is_none() {
        return Err("keine Antwort auf MEAS? und MEAS1?".into());
    }

    let model = idn.split(',').take(2).map(str::trim).collect::<Vec<_>>().join(" ");
    {
        let mut s = store.lock().unwrap();
        s.dmm.conn = ConnState::Connected(format!("{model} · Tasten am Gerät gesperrt"));
        if let Some(f) = &func {
            s.set_dmm_function(read_func(f));
        }
        if let Some(v) = first.as_deref().and_then(parse_reading) {
            s.push_dmm(v);
        }
    }
    ctx.request_repaint();
    let mut last_func_check = Instant::now();
    let mut last_poll = Instant::now();

    let result = (|| -> Result<(), String> {
        while !stopped(stop) {
            loop {
                match rx.try_recv() {
                    Ok(DmmCommand::SetFunction(f)) => {
                        if let Some(cmd) = f.scpi_conf() {
                            link.send(cmd).map_err(|e| e.to_string())?;
                            link.drain();
                            store.lock().unwrap().set_dmm_function(f);
                        }
                    }
                    Ok(DmmCommand::SetRate(r)) => {
                        link.send(r.scpi()).map_err(|e| e.to_string())?;
                        link.drain();
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Ok(()),
                }
            }

            if func_supported && last_func_check.elapsed() >= Duration::from_secs(1) {
                last_func_check = Instant::now();
                let f = read_func(&link.query("FUNC?").map_err(|e| e.to_string())?);
                store.lock().unwrap().set_dmm_function(f);
            }

            let wait = POLL_PERIOD.saturating_sub(last_poll.elapsed());
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
            last_poll = Instant::now();
            let answer = link.query(meas_cmd).map_err(|e| e.to_string())?;
            if let Some(v) = parse_reading(&answer) {
                store.lock().unwrap().push_dmm(v);
                ctx.request_repaint();
            }
        }
        Ok(())
    })();

    let _ = link.send("SYST:LOC");
    result
}

/// `powermeter --probe-xdm <port>` and the app's "Multimeter prüfen": asks
/// the meter a fixed list of questions, with and without remote mode, and
/// prints the raw answers. Changes no measurement setting; only `SYST:REM`
/// and finally `SYST:LOC` switch the remote mode.
pub fn probe_port(port: &str, baud: u32, out: &mut dyn io::Write) -> Result<(), String> {
    let _ = writeln!(out, "OWON XDM an {port}, {baud} Baud\n");
    let port = open_serial(port, baud)?;
    probe(port, out)
}

pub fn probe(mut port: Box<dyn serialport::SerialPort>, out: &mut dyn io::Write) -> Result<(), String> {
    std::thread::sleep(Duration::from_millis(150));
    let pending = collect(&mut port, Duration::from_millis(200));
    if !pending.is_empty() {
        let _ = writeln!(out, "Schon vorher im Puffer: {}", escape(&pending));
    }
    let steps: [(&str, &[u8]); 15] = [
        ("*IDN?", b"*IDN?\n"),
        ("FUNC?", b"FUNC?\n"),
        ("MEAS?", b"MEAS?\n"),
        ("MEAS1?", b"MEAS1?\n"),
        ("FUNC? mit CR LF", b"FUNC?\r\n"),
        ("SYST:REM", b"SYST:REM\n"),
        ("FUNC?", b"FUNC?\n"),
        ("MEAS?", b"MEAS?\n"),
        ("MEAS1?", b"MEAS1?\n"),
        ("RATE?", b"RATE?\n"),
        ("*IDN?", b"*IDN?\n"),
        ("SYST:LOC", b"SYST:LOC\n"),
        ("FUNC?", b"FUNC?\n"),
        ("MEAS?", b"MEAS?\n"),
        ("*IDN?", b"*IDN?\n"),
    ];
    let mut answered = 0;
    for (label, bytes) in steps {
        let t = Instant::now();
        port.write_all(bytes).and_then(|_| port.flush()).map_err(|e| format!("Senden: {e}"))?;
        let wait = if label.contains('?') { ANSWER_TIMEOUT } else { Duration::from_millis(300) };
        let got = collect(&mut port, wait);
        let ms = t.elapsed().as_millis();
        if got.is_empty() {
            let _ = writeln!(out, "→ {label:<16} ← keine Antwort");
        } else {
            answered += 1;
            let _ = writeln!(out, "→ {label:<16} ← {} ({ms} ms)", escape(&got));
        }
    }
    if answered == 0 {
        return Err("Das Multimeter antwortet auf gar nichts. Port und Baudrate (115200) prüfen.".into());
    }
    Ok(())
}

/// Everything that arrives until a line is complete and the line stays
/// quiet for 100 ms, or until `wait` is over.
fn collect(port: &mut Box<dyn serialport::SerialPort>, wait: Duration) -> Vec<u8> {
    let deadline = Instant::now() + wait;
    let mut got = Vec::new();
    let mut quiet_since: Option<Instant> = None;
    while Instant::now() < deadline {
        let mut chunk = [0u8; 256];
        match port.read(&mut chunk) {
            Ok(n) if n > 0 => {
                got.extend_from_slice(&chunk[..n]);
                quiet_since = None;
            }
            _ => {
                if got.ends_with(b"\n") {
                    let since = *quiet_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= Duration::from_millis(100) {
                        break;
                    }
                }
            }
        }
    }
    got
}

/// `"VOLT"\r\n` with the control characters made visible.
fn escape(bytes: &[u8]) -> String {
    let mut s = String::new();
    for &b in bytes {
        match b {
            b'\r' => s.push_str("\\r"),
            b'\n' => s.push_str("\\n"),
            0x20..=0x7e => s.push(b as char),
            _ => s.push_str(&format!("\\x{b:02X}")),
        }
    }
    s
}

/// XDM1041/XDM1241 firmware older than V4.3.0 reports CONT as DIOD and
/// DIOD as CONT in `FUNC?` (IDN: `OWON,XDM1241,<serial>,V4.2.0,3`).
pub fn swaps_diod_cont(idn: &str) -> bool {
    let parts: Vec<&str> = idn.trim().split(',').map(str::trim).collect();
    if parts.len() < 4
        || !parts[0].eq_ignore_ascii_case("OWON")
        || !(parts[1].eq_ignore_ascii_case("XDM1041") || parts[1].eq_ignore_ascii_case("XDM1241"))
    {
        return false;
    }
    let mut v = parts[3].trim_start_matches(['V', 'v']).split('.');
    match (v.next().and_then(|x| x.parse::<u32>().ok()), v.next().and_then(|x| x.parse::<u32>().ok())) {
        (Some(major), Some(minor)) => major < 4 || (major == 4 && minor < 3),
        _ => false,
    }
}

/// Parses a reading. Overload is reported as a huge number (≥ 1E9) or as
/// text like `OL`/`overload`; both map to NaN which the UI shows as "OL".
pub fn parse_reading(s: &str) -> Option<f64> {
    let t = s.trim().trim_matches('"');
    if t.is_empty() {
        return None;
    }
    if t.to_ascii_uppercase().contains("OL") || t.to_ascii_uppercase().contains("OVER") {
        return Some(f64::NAN);
    }
    let v: f64 = t.parse().ok()?;
    if v.abs() >= 9.0e8 { Some(f64::NAN) } else { Some(v) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::sync::Mutex;
    #[cfg(unix)]
    use std::sync::atomic::Ordering;

    #[test]
    fn parses_readings() {
        assert_eq!(parse_reading("8.924057E-01"), Some(0.8924057));
        assert_eq!(parse_reading("-1.2E-03\r"), Some(-0.0012));
        assert!(parse_reading("1E+9").unwrap().is_nan());
        assert!(parse_reading("OL").unwrap().is_nan());
        assert_eq!(parse_reading(""), None);
        assert_eq!(parse_reading("garbage"), None);
        assert!(parse_reading("-9.9E+37").unwrap().is_nan());
    }

    /// How the fake XDM behaves.
    #[cfg(unix)]
    #[derive(Clone, Copy)]
    struct Fake {
        /// Answers queries other than `*IDN?` only after `SYST:REM`, like
        /// the XDM1241 on Müco's bench.
        needs_remote: bool,
        answers_func: bool,
        answers_meas: bool,
        answers_meas1: bool,
    }

    /// A fake XDM on the other end of a pseudo terminal.
    #[cfg(unix)]
    fn fake_xdm(
        mut port: serialport::TTYPort,
        fake: Fake,
        log: Arc<Mutex<Vec<String>>>,
        done: Arc<AtomicBool>,
    ) -> std::thread::JoinHandle<()> {
        use std::io::{Read, Write};
        std::thread::spawn(move || {
            let mut remote = false;
            let mut buf = Vec::new();
            while !done.load(Ordering::Relaxed) {
                let mut chunk = [0u8; 256];
                match port.read(&mut chunk) {
                    Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
                    _ => continue,
                }
                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    let cmd = String::from_utf8_lossy(&line).trim().to_string();
                    log.lock().unwrap().push(cmd.clone());
                    let listening = remote || !fake.needs_remote;
                    let answer = match cmd.as_str() {
                        "*IDN?" => Some("OWON,XDM1241,2330123,V4.3.0,3"),
                        "SYST:REM" => {
                            remote = true;
                            None
                        }
                        "SYST:LOC" => {
                            remote = false;
                            None
                        }
                        "FUNC?" if listening && fake.answers_func => Some("\"VOLT\""),
                        "MEAS?" if listening && fake.answers_meas => Some("1.234567E+00"),
                        "MEAS1?" if listening && fake.answers_meas1 => Some("2.345678E+00"),
                        _ => None,
                    };
                    if let Some(a) = answer {
                        let _ = port.write_all(format!("{a}\r\n").as_bytes());
                    }
                }
            }
        })
    }

    /// Runs `job` on our end of a pty with the fake on the other end.
    #[cfg(unix)]
    fn with_fake<T>(fake: Fake, job: impl FnOnce(Box<dyn serialport::SerialPort>) -> T) -> (T, Vec<String>) {
        use serialport::SerialPort;
        let (mut master, mut slave) = serialport::TTYPort::pair().unwrap();
        master.set_timeout(Duration::from_millis(20)).ok();
        slave.set_timeout(Duration::from_millis(20)).unwrap();
        // A second handle keeps the pty open after the job drops its port,
        // so its last command (SYST:LOC) isn't lost on close.
        let keep_open = slave.try_clone_native().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let device = fake_xdm(master, fake, log.clone(), done.clone());
        let result = job(Box::new(slave));
        std::thread::sleep(Duration::from_millis(50));
        done.store(true, Ordering::Relaxed);
        device.join().unwrap();
        drop(keep_open);
        let log = log.lock().unwrap().clone();
        (result, log)
    }

    /// One session against the fake until a few readings arrived:
    /// (log, status label, readings, last reading).
    #[cfg(unix)]
    fn session_against(fake: Fake) -> (Vec<String>, String, usize, f64) {
        let store = crate::model::Store::shared();
        let ((), log) = with_fake(fake, |port| {
            let (_tx, rx) = std::sync::mpsc::channel();
            let stop = Arc::new(AtomicBool::new(false));
            let worker = {
                let (store, stop) = (store.clone(), stop.clone());
                std::thread::spawn(move || session(port, DmmRate::Fast, &store, &egui::Context::default(), &rx, &stop))
            };
            let deadline = Instant::now() + Duration::from_secs(8);
            while Instant::now() < deadline && store.lock().unwrap().dmm.samples.len() < 3 {
                std::thread::sleep(Duration::from_millis(10));
            }
            stop.store(true, Ordering::Relaxed);
            let result = worker.join().unwrap();
            assert!(result.is_ok(), "{result:?}");
        });
        let s = store.lock().unwrap();
        let label = match &s.dmm.conn {
            ConnState::Connected(m) => m.clone(),
            other => format!("{other:?}"),
        };
        (log, label, s.dmm.samples.len(), s.dmm.samples.back().map_or(f64::NAN, |x| x.value))
    }

    #[cfg(unix)]
    #[test]
    fn remote_comes_before_the_first_question() {
        let fake = Fake { needs_remote: true, answers_func: true, answers_meas: true, answers_meas1: true };
        let (log, label, n, last) = session_against(fake);
        assert!(n >= 3, "{log:?}");
        assert_eq!(last, 1.234567);
        assert_eq!(&log[..3], ["*IDN?", "SYST:REM", "RATE F"], "{log:?}");
        assert_eq!(label, "OWON XDM1241 · Tasten am Gerät gesperrt");
        assert_eq!(log.last().map(String::as_str), Some("SYST:LOC"), "{log:?}");
    }

    #[cfg(unix)]
    #[test]
    fn works_without_func_and_with_meas1_only() {
        let fake = Fake { needs_remote: true, answers_func: false, answers_meas: false, answers_meas1: true };
        let (log, _, n, last) = session_against(fake);
        assert!(n >= 3, "{log:?}");
        assert_eq!(last, 2.345678);
        assert_eq!(log.iter().filter(|c| *c == "FUNC?").count(), 1, "FUNC? asked once only: {log:?}");
        assert_eq!(log.iter().filter(|c| *c == "MEAS?").count(), 1, "{log:?}");
    }

    #[cfg(unix)]
    #[test]
    fn probe_shows_the_raw_answers() {
        let fake = Fake { needs_remote: true, answers_func: true, answers_meas: true, answers_meas1: false };
        let (result, log) = with_fake(fake, |port| {
            let mut out = Vec::new();
            let r = probe(port, &mut out);
            (r, String::from_utf8(out).unwrap())
        });
        let (r, text) = result;
        assert!(r.is_ok(), "{r:?}");
        assert!(text.contains("→ *IDN?            ← OWON,XDM1241,2330123,V4.3.0,3\\r\\n"), "{text}");
        let lines: Vec<&str> = text.lines().filter(|l| l.starts_with("→ FUNC?  ")).collect();
        assert!(lines[0].ends_with("keine Antwort"), "{text}");
        assert!(lines[1].contains(r#""VOLT"\r\n"#), "{text}");
        assert_eq!(log.last().map(String::as_str), Some("*IDN?"), "{log:?}");
        assert!(log.contains(&"SYST:LOC".to_string()));
    }

    #[test]
    fn old_firmware_swaps_diod_and_cont() {
        assert!(swaps_diod_cont("OWON,XDM1041,23362355,V4.2.0,3"));
        assert!(swaps_diod_cont("OWON,XDM1241,1234,V3.9.1,2\r"));
        assert!(!swaps_diod_cont("OWON,XDM1241,1234,V4.3.0,3"));
        assert!(!swaps_diod_cont("OWON,XDM1241,1234,V5.0.0,3"));
        assert!(!swaps_diod_cont("OWON,XDM2041,1234,V4.2.0,3"));
        assert!(!swaps_diod_cont("OWON,XDM1241"));
    }
}
