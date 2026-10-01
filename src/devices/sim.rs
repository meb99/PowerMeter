//! Simulated devices, so the app, graphs and OBS overlay can be tried out
//! before any hardware is on the bench.
//!
//! The power simulator models a 19 V / 1 A bench supply feeding a notebook
//! mainboard that boots, loads up, runs into the current limit, has a flaky
//! contact and finally a short – the things a repair video wants to show.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui;

use super::{DmmCommand, stopped};
use crate::model::{ConnState, DmmFunction, Shared};

pub const SIM_VSET: f64 = 19.0;
pub const SIM_ISET: f64 = 1.0;
const SCENARIO_LEN: f64 = 40.0;

/// Tiny xorshift PRNG; good enough for measurement noise.
struct Noise(u64);

impl Noise {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    }
}

/// Load resistance (Ω) of the simulated device at scenario time `t`.
/// `None` means the output is open (nothing connected).
fn load_at(t: f64) -> Option<f64> {
    match t {
        t if t < 3.0 => None,
        // plug in: input capacitors charge -> inrush hits the current limit
        t if t < 3.04 => Some(2.0),
        t if t < 8.0 => Some(42.0),
        // booting: load wobbles
        t if t < 14.0 => Some(25.0 + 4.0 * (t * 7.0).sin() + 2.0 * (t * 23.0).sin()),
        // heavy load -> supply goes into CC
        t if t < 18.0 => Some(15.0),
        t if t < 21.0 => Some(26.0),
        t if t < 25.0 => Some(26.0),
        // short circuit
        t if t < 26.5 => Some(0.05),
        t if t < 35.0 => Some(30.0 + 3.0 * (t * 3.0).sin()),
        _ => None,
    }
}

/// Output of an ideal CV/CC supply with a little droop, ripple and noise.
fn psu_output(t: f64, load: Option<f64>, noise: &mut Noise) -> (f64, f64) {
    let ripple = 0.004 * (t * 2.0 * std::f64::consts::PI * 100.0).sin();
    let (v, i) = match load {
        None => (SIM_VSET, 0.0),
        Some(r) => {
            let i_cv = SIM_VSET / r;
            if i_cv >= SIM_ISET {
                (SIM_ISET * r, SIM_ISET)
            } else {
                let droop = 0.01 * i_cv;
                ((SIM_VSET - droop), (SIM_VSET - droop) / r)
            }
        }
    };
    // 21.00–21.08 s: the supply's output sags for 80 ms (flaky contact /
    // supply hiccup) – voltage and, with a resistive load, current drop.
    let sag = if (21.0..21.08).contains(&t) { 0.7 } else { 1.0 };
    ((v + ripple) * sag + 0.0008 * noise.next(), (i * sag + 0.0002 * noise.next()).max(0.0))
}

pub fn run_power(store: Shared, ctx: egui::Context, rate_hz: f64, stop: Arc<AtomicBool>) {
    store.lock().unwrap().power.conn = ConnState::Connected(format!("Simulator {SIM_VSET} V / {SIM_ISET} A"));
    let mut noise = Noise(0x9E37_79B9_7F4A_7C15);
    let start = Instant::now();
    let period = Duration::from_secs_f64(1.0 / rate_hz);
    let mut next = Instant::now();
    while !stopped(&stop) {
        let t = start.elapsed().as_secs_f64() % SCENARIO_LEN;
        let (v, i) = psu_output(t, load_at(t), &mut noise);
        store.lock().unwrap().push_power(v, i);
        ctx.request_repaint();
        next += period;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
    store.lock().unwrap().power.conn = ConnState::Disconnected;
}

pub fn run_dmm(store: Shared, ctx: egui::Context, rx: Receiver<DmmCommand>, stop: Arc<AtomicBool>) {
    {
        let mut s = store.lock().unwrap();
        s.dmm.conn = ConnState::Connected("Simulator XDM1241".into());
        s.set_dmm_function(DmmFunction::VoltDc);
    }
    let mut noise = Noise(0xD1B5_4A32_D192_ED03);
    let start = Instant::now();
    let mut interval = Duration::from_millis(40);
    while !stopped(&stop) {
        loop {
            match rx.try_recv() {
                Ok(DmmCommand::SetFunction(f)) => store.lock().unwrap().set_dmm_function(f),
                Ok(DmmCommand::SetRate(r)) => {
                    interval = Duration::from_millis(match r {
                        super::DmmRate::Fast => 40,
                        super::DmmRate::Medium => 200,
                        super::DmmRate::Slow => 500,
                    })
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        let t = start.elapsed().as_secs_f64();
        let f = store.lock().unwrap().dmm.function;
        let n = noise.next();
        let value = match f {
            // A 3.3 V rail that sags a bit periodically.
            DmmFunction::VoltDc => 3.3012 - 0.02 * ((t * 0.7).sin().max(0.0)).powi(4) + 0.00015 * n,
            DmmFunction::VoltAc => 230.4 + 0.8 * (t * 0.1).sin() + 0.05 * n,
            DmmFunction::CurrDc => 0.1234 + 0.01 * (t * 1.3).sin() + 0.00005 * n,
            DmmFunction::CurrAc => 0.05 + 0.0001 * n,
            DmmFunction::Res => 4_700.0 + 1.5 * n,
            DmmFunction::Cont => {
                if (t as u64) % 4 < 2 {
                    0.4 + 0.01 * n
                } else {
                    f64::NAN
                }
            }
            DmmFunction::Diode => 0.6123 + 0.0003 * n,
            DmmFunction::Cap => 1.0e-4 * (1.0 + 0.001 * n),
            DmmFunction::Freq => 50.0 + 0.02 * n,
            DmmFunction::Period => 0.02 + 0.000_01 * n,
            DmmFunction::Temp => 42.0 + 3.0 * (t * 0.05).sin() + 0.05 * n,
            DmmFunction::Unknown => 0.0,
        };
        store.lock().unwrap().push_dmm(value);
        ctx.request_repaint();
        std::thread::sleep(interval);
    }
    store.lock().unwrap().dmm.conn = ConnState::Disconnected;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supply_model_switches_cv_cc() {
        let mut n = Noise(1);
        let (v, i) = psu_output(0.0, None, &mut n);
        assert!((v - SIM_VSET).abs() < 0.01 && i < 0.001);
        let (v, i) = psu_output(0.0, Some(38.0), &mut n);
        assert!(v > 18.9 && (i - 0.5).abs() < 0.01);
        let (v, i) = psu_output(0.0, Some(10.0), &mut n);
        assert!((i - SIM_ISET).abs() < 0.001 && (v - 10.0).abs() < 0.01);
    }
}
