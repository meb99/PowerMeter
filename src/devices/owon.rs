//! OWON XDM1041 / XDM1241 / XDM2041 bench multimeter over USB serial (SCPI).
//!
//! The meter answers `MEAS1?` with the value on the main display in plain
//! scientific notation (`8.924057E-01`). We poll as fast as the meter answers,
//! which with `RATE F` is the meter's own update rate, and check `FUNC?` once
//! a second so turning the knob / pressing a key on the meter is picked up.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui;

use super::{DmmCommand, DmmRate, read_line, stopped};
use crate::model::{ConnState, DmmFunction, Shared};

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
        match session(&cfg, &store, &ctx, &rx, &stop) {
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

    fn query(&mut self, cmd: &str) -> std::io::Result<String> {
        self.send(cmd)?;
        read_line(&mut self.port, &mut self.buf, Duration::from_millis(1500))?
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
    cfg: &OwonConfig,
    store: &Shared,
    ctx: &egui::Context,
    rx: &Receiver<DmmCommand>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let port = serialport::new(&cfg.port, cfg.baud)
        .timeout(Duration::from_millis(20))
        .open()
        .map_err(|e| format!("{}: {e}", cfg.port))?;
    let mut link = Link { port, buf: Vec::new() };
    link.drain();

    let idn = link.query("*IDN?").map_err(|e| e.to_string())?;
    link.send("SYST:REM").map_err(|e| e.to_string())?;
    link.send(cfg.rate.scpi()).map_err(|e| e.to_string())?;
    link.drain();

    let model = idn.split(',').take(2).collect::<Vec<_>>().join(" ");
    {
        let mut s = store.lock().unwrap();
        s.dmm.conn = ConnState::Connected(model);
    }
    let mut last_func_check = Instant::now() - Duration::from_secs(10);

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
                let f = DmmFunction::from_scpi(&link.query("FUNC?").map_err(|e| e.to_string())?);
                store.lock().unwrap().set_dmm_function(f);
            }

            let answer = link.query("MEAS1?").map_err(|e| e.to_string())?;
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

    #[test]
    fn parses_readings() {
        assert_eq!(parse_reading("8.924057E-01"), Some(0.8924057));
        assert_eq!(parse_reading("-1.2E-03\r"), Some(-0.0012));
        assert!(parse_reading("1E+9").unwrap().is_nan());
        assert!(parse_reading("OL").unwrap().is_nan());
        assert_eq!(parse_reading(""), None);
        assert_eq!(parse_reading("garbage"), None);
    }
}
