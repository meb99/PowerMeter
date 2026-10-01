//! In-line DC power meter ("PowerMon"): a small microcontroller with an INA228
//! between bench supply and device under test, streaming lines over USB.
//!
//! Protocol (one line per sample, see `firmware/powermon`):
//!
//! ```text
//! PM,<device_micros>,<volts>,<amps>
//! # comment / info line
//! ```
//!
//! Plain `<volts>,<amps>` lines are accepted too, so any MCU + sensor combo
//! that prints two numbers works.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use eframe::egui;

use super::{read_line, stopped};
use crate::model::{ConnState, PowerSample, Shared};

pub struct PowerMonConfig {
    pub port: String,
    pub baud: u32,
}

#[derive(Debug, PartialEq)]
pub enum Line {
    Sample { device_us: Option<u64>, v: f64, i: f64 },
    Info(String),
}

pub fn parse_line(line: &str) -> Option<Line> {
    let l = line.trim();
    if l.is_empty() {
        return None;
    }
    if let Some(info) = l.strip_prefix('#') {
        return Some(Line::Info(info.trim().to_string()));
    }
    let body = l.strip_prefix("PM,").unwrap_or(l);
    let parts: Vec<&str> = body.split([',', ';', '\t']).map(str::trim).collect();
    match parts.as_slice() {
        [t, v, i] => Some(Line::Sample { device_us: t.parse().ok(), v: v.parse().ok()?, i: i.parse().ok()? }),
        [v, i] => Some(Line::Sample { device_us: None, v: v.parse().ok()?, i: i.parse().ok()? }),
        _ => None,
    }
}

/// Maps device timestamps onto the host time axis. Using the device clock
/// removes USB scheduling jitter from the graph; the offset is re-anchored if
/// the two clocks drift apart (or the MCU resets).
#[derive(Default)]
struct Clock {
    offset: Option<f64>,
}

impl Clock {
    fn map(&mut self, device_us: u64, host_t: f64) -> f64 {
        let dev = device_us as f64 / 1e6;
        let t = match self.offset {
            Some(off) => dev + off,
            None => host_t,
        };
        // A sample can't arrive before it was taken; if it seems to, or we
        // lag more than 50 ms behind, re-anchor.
        if self.offset.is_none() || t > host_t || host_t - t > 0.05 {
            self.offset = Some(host_t - dev);
            return host_t;
        }
        t
    }
}

pub fn run(cfg: PowerMonConfig, store: Shared, ctx: egui::Context, stop: Arc<AtomicBool>) {
    let set_conn = |c: ConnState| {
        store.lock().unwrap().power.conn = c;
        ctx.request_repaint();
    };
    set_conn(ConnState::Connecting);
    while !stopped(&stop) {
        if let Err(e) = session(&cfg, &store, &ctx, &stop) {
            set_conn(ConnState::Error(format!("{e} – neuer Versuch …")));
            let until = Instant::now() + Duration::from_secs(2);
            while Instant::now() < until && !stopped(&stop) {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    set_conn(ConnState::Disconnected);
}

fn session(cfg: &PowerMonConfig, store: &Shared, ctx: &egui::Context, stop: &AtomicBool) -> Result<(), String> {
    let mut port = serialport::new(&cfg.port, cfg.baud)
        .timeout(Duration::from_millis(20))
        .open()
        .map_err(|e| format!("{}: {e}", cfg.port))?;
    // Native-USB boards only start streaming once DTR is asserted.
    let _ = port.write_data_terminal_ready(true);
    let _ = port.write_all(b"INFO\n");

    let mut buf = Vec::new();
    let mut clock = Clock::default();
    let mut info = String::from("verbunden");
    let mut last_data = Instant::now();
    store.lock().unwrap().power.conn = ConnState::Connected(info.clone());

    while !stopped(stop) {
        let line = read_line(&mut port, &mut buf, Duration::from_millis(100)).map_err(|e| e.to_string())?;
        let Some(line) = line else {
            if last_data.elapsed() > Duration::from_secs(3) {
                return Err("keine Daten vom Messmodul".into());
            }
            continue;
        };
        match parse_line(&line) {
            Some(Line::Sample { device_us, v, i }) => {
                last_data = Instant::now();
                let mut s = store.lock().unwrap();
                let host_t = s.now();
                let t = device_us.map_or(host_t, |us| clock.map(us, host_t));
                s.push_power_sample(PowerSample { t, v, i });
                drop(s);
                ctx.request_repaint();
            }
            Some(Line::Info(text)) => {
                info = text;
                store.lock().unwrap().power.conn = ConnState::Connected(info.clone());
            }
            None => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_protocol_lines() {
        assert_eq!(
            parse_line("PM,123456,19.0012,0.98765"),
            Some(Line::Sample { device_us: Some(123456), v: 19.0012, i: 0.98765 })
        );
        assert_eq!(parse_line("5.02, 0.1"), Some(Line::Sample { device_us: None, v: 5.02, i: 0.1 }));
        assert_eq!(parse_line("# PowerMon INA228"), Some(Line::Info("PowerMon INA228".into())));
        assert_eq!(parse_line("hello"), None);
        assert_eq!(parse_line(""), None);
    }

    #[test]
    fn clock_follows_device_time() {
        let mut c = Clock::default();
        assert_eq!(c.map(1_000_000, 10.0), 10.0);
        // 10 ms later on the device, arriving 3 ms late on the host
        assert!((c.map(1_010_000, 10.013) - 10.010).abs() < 1e-9);
        // device reset: timestamps jump back -> re-anchor to host time
        assert_eq!(c.map(5_000, 10.1), 10.1);
    }
}
