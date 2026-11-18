//! OWON XDM1041 / XDM1241 / XDM2041 bench multimeter over USB serial (SCPI).
//!
//! The meter answers `MEAS?` with the value on the main display in plain
//! scientific notation (`8.924057E-01`). We poll as fast as the meter answers,
//! which with `RATE F` is the meter's own update rate, and check `FUNC?` once
//! a second so pressing a key on the meter is picked up.
//!
//! We don't send `SYST:REM`: it locks the meter's keys, and the user should
//! still be able to switch the function on the meter while filming.
//! XDM1041/XDM1241 firmware before V4.3.0 swaps DIOD and CONT in the `FUNC?`
//! reply (setting them with `CONF:` works); [`swaps_diod_cont`] undoes that.

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
    let swap_diod_cont = swaps_diod_cont(&idn);
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
                let f = match f {
                    DmmFunction::Cont if swap_diod_cont => DmmFunction::Diode,
                    DmmFunction::Diode if swap_diod_cont => DmmFunction::Cont,
                    f => f,
                };
                store.lock().unwrap().set_dmm_function(f);
            }

            let answer = link.query("MEAS?").map_err(|e| e.to_string())?;
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
