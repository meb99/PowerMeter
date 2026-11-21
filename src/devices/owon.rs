//! OWON XDM1041 / XDM1241 / XDM2041 bench multimeter over USB serial (SCPI).
//!
//! The meter answers `MEAS?` with the value on the main display in plain
//! scientific notation (`8.924057E-01`). We poll every [`POLL_PERIOD`] and
//! check `FUNC?` once a second so pressing a key on the meter is picked up.
//!
//! Remote mode: `SYST:REM` locks the meter's keys, so we first try without
//! it. If the meter then doesn't answer `FUNC?` (seen on a real XDM1241 after
//! `SYST:LOC`), we send `SYST:REM` and stay in remote mode for this
//! connection; the status line says the keys are locked. `SYST:LOC` is only
//! sent when we switched to remote mode ourselves, and only when leaving.
//! XDM1041/XDM1241 firmware before V4.3.0 swaps DIOD and CONT in the `FUNC?`
//! reply (setting them with `CONF:` works); [`swaps_diod_cont`] undoes that.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui;

use super::{DmmCommand, DmmRate, read_line, stopped};
use crate::model::{ConnState, DmmFunction, Shared};

/// Shortest time between two `MEAS?` (at most 50 readings per second, as
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

    // Once the meter needed remote mode, keep it for the reconnects.
    let mut remote = false;
    // Reconnect loop: unplugging the meter mid-video shouldn't need a click.
    while !stopped(&stop) {
        let opened = serialport::new(&cfg.port, cfg.baud)
            .timeout(Duration::from_millis(20))
            .open()
            .map_err(|e| format!("{}: {e}", cfg.port));
        let result = opened.and_then(|port| session(port, cfg.rate, &mut remote, &store, &ctx, &rx, &stop));
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

struct Link {
    port: Box<dyn serialport::SerialPort>,
    buf: Vec<u8>,
}

impl Link {
    fn send(&mut self, cmd: &str) -> std::io::Result<()> {
        self.port.write_all(cmd.as_bytes())?;
        self.port.write_all(b"\n")?;
        self.port.flush()
    }

    /// The answer, or `None` when the meter stays silent.
    fn try_query(&mut self, cmd: &str) -> std::io::Result<Option<String>> {
        self.send(cmd)?;
        read_line(&mut self.port, &mut self.buf, ANSWER_TIMEOUT)
    }

    fn query(&mut self, cmd: &str) -> std::io::Result<String> {
        self.try_query(cmd)?
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::TimedOut, format!("keine Antwort auf {cmd}")))
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
    remote: &mut bool,
    store: &Shared,
    ctx: &egui::Context,
    rx: &Receiver<DmmCommand>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let mut link = Link { port, buf: Vec::new() };
    link.drain();

    let idn = link.query("*IDN?").map_err(|e| e.to_string())?;
    let swap_diod_cont = swaps_diod_cont(&idn);
    link.send(rate.scpi()).map_err(|e| e.to_string())?;
    if *remote {
        link.send("SYST:REM").map_err(|e| e.to_string())?;
    }
    link.drain();

    let mut func = link.try_query("FUNC?").map_err(|e| e.to_string())?;
    if func.is_none() && !*remote {
        // This meter only answers in remote mode.
        link.send("SYST:REM").map_err(|e| e.to_string())?;
        link.drain();
        *remote = true;
        func = link.try_query("FUNC?").map_err(|e| e.to_string())?;
    }
    let Some(func) = func else {
        return Err("keine Antwort auf FUNC?".into());
    };
    let read_func = |answer: &str| match DmmFunction::from_scpi(answer) {
        DmmFunction::Cont if swap_diod_cont => DmmFunction::Diode,
        DmmFunction::Diode if swap_diod_cont => DmmFunction::Cont,
        f => f,
    };

    let mut model = idn.split(',').take(2).map(str::trim).collect::<Vec<_>>().join(" ");
    if *remote {
        model.push_str(" · Tasten am Gerät gesperrt");
    }
    {
        let mut s = store.lock().unwrap();
        s.dmm.conn = ConnState::Connected(model);
        s.set_dmm_function(read_func(&func));
    }
    ctx.request_repaint();
    let mut last_func_check = Instant::now();
    let mut last_poll = Instant::now() - POLL_PERIOD;

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

            if last_func_check.elapsed() >= Duration::from_secs(1) {
                last_func_check = Instant::now();
                let Some(answer) = link.try_query("FUNC?").map_err(|e| e.to_string())? else {
                    return Err(silent(remote, "FUNC?"));
                };
                store.lock().unwrap().set_dmm_function(read_func(&answer));
            }

            let wait = POLL_PERIOD.saturating_sub(last_poll.elapsed());
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
            last_poll = Instant::now();
            let Some(answer) = link.try_query("MEAS?").map_err(|e| e.to_string())? else {
                return Err(silent(remote, "MEAS?"));
            };
            if let Some(v) = parse_reading(&answer) {
                store.lock().unwrap().push_dmm(v);
                ctx.request_repaint();
            }
        }
        Ok(())
    })();

    if *remote {
        let _ = link.send("SYST:LOC");
    }
    result
}

/// The meter stopped answering: without remote mode that is the meter
/// wanting it, so the next attempt uses `SYST:REM`.
fn silent(remote: &mut bool, cmd: &str) -> String {
    *remote = true;
    format!("keine Antwort auf {cmd}")
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

    /// A fake XDM on the other end of a pseudo terminal. With
    /// `answers_local == false` it answers only `*IDN?` until `SYST:REM`.
    #[cfg(unix)]
    fn fake_xdm(
        mut port: serialport::TTYPort,
        answers_local: bool,
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
                        "FUNC?" if remote || answers_local => Some("\"VOLT\""),
                        "MEAS?" if remote || answers_local => Some("1.234567E+00"),
                        _ => None,
                    };
                    if let Some(a) = answer {
                        let _ = port.write_all(format!("{a}\r\n").as_bytes());
                    }
                }
            }
        })
    }

    /// Runs one session against the fake until a few readings arrived.
    #[cfg(unix)]
    fn session_against_fake(answers_local: bool) -> (Vec<String>, String, bool, usize) {
        use serialport::SerialPort;
        let (mut master, mut slave) = serialport::TTYPort::pair().unwrap();
        master.set_timeout(Duration::from_millis(20)).ok();
        slave.set_timeout(Duration::from_millis(20)).unwrap();
        // A second handle keeps the pty open after the session drops its
        // port, so its last command (SYST:LOC) isn't lost on close.
        let keep_open = slave.try_clone_native().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let fake = fake_xdm(master, answers_local, log.clone(), done.clone());

        let store = crate::model::Store::shared();
        let (_tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let (store, stop) = (store.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut remote = false;
                let r =
                    session(Box::new(slave), DmmRate::Fast, &mut remote, &store, &egui::Context::default(), &rx, &stop);
                (r, remote)
            })
        };
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut label = String::new();
        while Instant::now() < deadline {
            {
                let s = store.lock().unwrap();
                if let ConnState::Connected(m) = &s.dmm.conn {
                    label = m.clone();
                }
                if s.dmm.samples.len() >= 3 {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
        let (result, remote) = worker.join().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        done.store(true, Ordering::Relaxed);
        fake.join().unwrap();
        drop(keep_open);
        assert!(result.is_ok(), "{result:?}");
        let samples = store.lock().unwrap().dmm.samples.len();
        let log = log.lock().unwrap().clone();
        (log, label, remote, samples)
    }

    #[cfg(unix)]
    #[test]
    fn keys_stay_free_when_the_meter_answers_without_remote() {
        let (log, label, remote, samples) = session_against_fake(true);
        assert!(samples >= 3);
        assert!(!remote);
        assert!(!log.iter().any(|c| c.starts_with("SYST:")), "{log:?}");
        assert_eq!(label, "OWON XDM1241");
    }

    #[cfg(unix)]
    #[test]
    fn falls_back_to_remote_when_the_meter_stays_silent() {
        let (log, label, remote, samples) = session_against_fake(false);
        assert!(samples >= 3, "{log:?}");
        assert!(remote);
        assert!(label.contains("Tasten am Gerät gesperrt"), "{label}");
        let rem = log.iter().position(|c| c == "SYST:REM").expect("SYST:REM sent");
        assert!(log[..rem].contains(&"FUNC?".to_string()));
        assert_eq!(log.last().map(String::as_str), Some("SYST:LOC"), "{log:?}");
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
