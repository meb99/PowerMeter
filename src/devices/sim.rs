//! Simulated devices, so the app, graphs and OBS overlay can be tried out
//! before any hardware is on the bench.
//!
//! The power simulator models a 19 V / 1 A bench supply feeding a notebook
//! mainboard that boots, loads up, runs into the current limit, has a flaky
//! contact and finally a short – the things a repair video wants to show.
//!
//! [`FakeSpm`] goes one level deeper: it is an OWON SPM6103 at the SCPI
//! level, feeding the same load scenario, so the real SPM driver can run
//! without the hardware.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use eframe::egui;

use super::owon_spm::{self, SpmConfig};
use super::{DmmCommand, PsuCommand, ScpiPort, stopped};
use crate::format;
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
    psu_output_with(SIM_VSET, SIM_ISET, t, load, noise)
}

/// The same for any set point.
fn psu_output_with(v_set: f64, i_set: f64, t: f64, load: Option<f64>, noise: &mut Noise) -> (f64, f64) {
    let ripple = 0.004 * (t * 2.0 * std::f64::consts::PI * 100.0).sin();
    let (v, i) = match load {
        None => (v_set, 0.0),
        Some(r) => {
            let i_cv = v_set / r;
            if i_cv >= i_set {
                (i_set * r, i_set)
            } else {
                let droop = 0.01 * i_cv;
                ((v_set - droop), (v_set - droop) / r)
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
        let value = dmm_value(f, t, noise.next());
        store.lock().unwrap().push_dmm(value);
        ctx.request_repaint();
        std::thread::sleep(interval);
    }
    store.lock().unwrap().dmm.conn = ConnState::Disconnected;
}

/// What the simulated multimeter reads at time `t`; `n` is noise in [-1, 1].
fn dmm_value(f: DmmFunction, t: f64, n: f64) -> f64 {
    match f {
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
    }
}

// ------------------------------------------------------------- OWON SPM

const SPM_IDN: &str = "OWON,SPM6103,SIM00001,FV:V2.0.0";
const SPM_V_MAX: f64 = 60.0;
const SPM_I_MAX: f64 = 10.0;
/// Output slew rate after switching on or a new set point.
const SPM_SLEW_V_PER_S: f64 = 5.5;
/// The SPM seems to refresh its own readings only this often.
const SPM_REFRESH: Duration = Duration::from_millis(300);
const SPM_REPLY_DELAY: Duration = Duration::from_millis(15);
/// How late a "late" answer comes (test hook `late_replies`): after the
/// driver's first wait (300 ms) has run out.
const SPM_LATE_DELAY: Duration = Duration::from_millis(400);
/// Commands closer together than this are lost.
const SPM_MIN_GAP: Duration = Duration::from_millis(5);
/// A multimeter function switch takes this long.
const SPM_FUNC_SWITCH: Duration = Duration::from_millis(800);

/// The simulated supply. Shared between the [`FakeSpm`] port(s) and tests,
/// and it outlives reconnects like a real device would.
pub struct SpmDevice {
    start: Instant,
    /// Answer to `*IDN?`.
    pub idn: &'static str,
    pub v_set: f64,
    pub i_set: f64,
    pub ovp: f64,
    pub ocp: f64,
    /// Starts on, as if someone had pressed the button on the front.
    pub output_on: bool,
    pub trip_ovp: bool,
    pub trip_ocp: bool,
    /// Where the ramping output is heading from.
    v_out: f64,
    last_step: Instant,
    /// Readings frozen between refreshes: (V, I, mode).
    reading: (f64, f64, u8),
    reading_at: Option<Instant>,
    func: DmmFunction,
    func_switch: Option<(DmmFunction, Instant)>,
    noise: Noise,
    replies: VecDeque<(Instant, Vec<u8>)>,
    last_cmd: Option<Instant>,
    /// Test hook: swallow the answers to this many queries.
    pub drop_replies: u32,
    /// Test hook: answer this many queries late (busy after a state change).
    pub late_replies: u32,
    /// The last commands received.
    pub log: VecDeque<String>,
}

pub type SharedSpm = Arc<Mutex<SpmDevice>>;

impl SpmDevice {
    pub fn shared() -> SharedSpm {
        let now = Instant::now();
        Arc::new(Mutex::new(SpmDevice {
            start: now,
            idn: SPM_IDN,
            v_set: SIM_VSET,
            i_set: SIM_ISET,
            ovp: 61.0,
            ocp: 10.5,
            output_on: true,
            trip_ovp: false,
            trip_ocp: false,
            v_out: SIM_VSET,
            last_step: now,
            reading: (0.0, 0.0, 0),
            reading_at: None,
            func: DmmFunction::VoltDc,
            func_switch: None,
            noise: Noise(0x2545_F491_4F6C_DD1D),
            replies: VecDeque::new(),
            last_cmd: None,
            drop_replies: 0,
            late_replies: 0,
            log: VecDeque::new(),
        }))
    }

    /// One received line.
    fn command(&mut self, line: &str) {
        let now = Instant::now();
        self.log.push_back(line.to_string());
        if self.log.len() > 1_000 {
            self.log.pop_front();
        }
        let too_fast = self.last_cmd.is_some_and(|l| now - l < SPM_MIN_GAP);
        self.last_cmd = Some(now);
        if too_fast {
            return; // the real one chokes and loses it
        }
        // Only the first command of a `;` chain runs.
        let cmd = line.split(';').next().unwrap_or_default().trim().to_ascii_uppercase();
        let Some(reply) = self.execute(&cmd, now) else { return };
        if cmd.ends_with('?') && self.drop_replies > 0 {
            self.drop_replies -= 1;
            return;
        }
        let mut delay = SPM_REPLY_DELAY;
        if cmd.ends_with('?') && self.late_replies > 0 {
            self.late_replies -= 1;
            delay = SPM_LATE_DELAY;
        }
        // Answers leave in order, so a late one holds up those behind it.
        let at = self.replies.back().map_or(now + delay, |(last, _)| (*last).max(now + delay));
        self.replies.push_back((at, format!("{reply}\r\n").into_bytes()));
    }

    /// Runs a command; `Some` is the answer (sets answer nothing or `ERR`).
    fn execute(&mut self, cmd: &str, now: Instant) -> Option<String> {
        self.step(now);
        let onoff = |b: bool| if b { "ON" } else { "OFF" };
        let set = |arg: &str, max: f64| arg.trim().parse::<f64>().ok().filter(|v| (0.0..=max).contains(v));
        match cmd {
            "*IDN?" => return Some(self.idn.into()),
            "MEAS:ALL:INFO?" => {
                let (v, i, mode) = self.reading(now);
                return Some(format!(
                    "{v:.3},{i:.3},{:.3},{},{},OFF,{mode}",
                    v * i,
                    onoff(self.trip_ovp),
                    onoff(self.trip_ocp)
                ));
            }
            "MEAS:ALL?" => {
                let (v, i, _) = self.reading(now);
                return Some(format!("{v:.3},{i:.3}"));
            }
            "MEAS:VOLT?" => return Some(format!("{:.3}", self.reading(now).0)),
            "MEAS:CURR?" => return Some(format!("{:.3}", self.reading(now).1)),
            "VOLT?" => return Some(format!("{:.3}", self.v_set)),
            "CURR?" => return Some(format!("{:.3}", self.i_set)),
            "VOLT:LIM?" => return Some(format!("{:.3}", self.ovp)),
            "CURR:LIM?" => return Some(format!("{:.3}", self.ocp)),
            "OUTP?" => return Some(onoff(self.output_on).into()),
            "OUTP ON" | "OUTP 1" => {
                self.output_on = true;
                self.trip_ovp = false;
                self.trip_ocp = false;
                self.reading_at = None;
                return None;
            }
            "OUTP OFF" | "OUTP 0" => {
                self.output_on = false;
                self.reading_at = None;
                return None;
            }
            "SYST:REM" | "SYST:LOC" => return None,
            "FUNC?" => return Some(spm_func_name(self.dmm_function(now)).into()),
            "CONF:ALL?" => {
                let f = self.dmm_function(now);
                let value = dmm_value(f, (now - self.start).as_secs_f64(), self.noise.next());
                return Some(spm_conf_all(f, value));
            }
            _ => {}
        }
        let ok = if let Some(a) = cmd.strip_prefix("VOLT:LIM ") {
            set(a, SPM_V_MAX * 1.1).map(|v| self.ovp = v)
        } else if let Some(a) = cmd.strip_prefix("CURR:LIM ") {
            set(a, SPM_I_MAX * 1.1).map(|i| self.ocp = i)
        } else if let Some(a) = cmd.strip_prefix("VOLT ") {
            set(a, SPM_V_MAX).map(|v| self.v_set = v)
        } else if let Some(a) = cmd.strip_prefix("CURR ") {
            set(a, SPM_I_MAX).map(|i| self.i_set = i)
        } else if cmd.starts_with("FUNC:") {
            DmmFunction::SPM_SELECTABLE
                .into_iter()
                .find(|f| f.scpi_func_spm() == Some(cmd))
                .map(|f| self.func_switch = Some((f, now + SPM_FUNC_SWITCH)))
        } else {
            None // unknown command, *RST included
        };
        ok.is_none().then(|| "ERR".into())
    }

    /// Moves the output towards its target at the slew rate.
    fn step(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.last_step).as_secs_f64();
        self.last_step = self.last_step.max(now);
        if !self.output_on {
            self.v_out = 0.0;
            return;
        }
        let max = SPM_SLEW_V_PER_S * dt;
        self.v_out += (self.v_set - self.v_out).clamp(-max, max);
    }

    /// The displayed readings, refreshed every 0.3 s and quantized to
    /// 10 mV / 1 mA like the real one.
    fn reading(&mut self, now: Instant) -> (f64, f64, u8) {
        if self.reading_at.is_some_and(|at| now - at < SPM_REFRESH) {
            return self.reading;
        }
        self.reading_at = Some(now);
        let t = (now - self.start).as_secs_f64() % SCENARIO_LEN;
        let (mut v, mut i, mut mode) = (0.0, 0.0, 0);
        if self.output_on {
            let load = load_at(t);
            (v, i) = psu_output_with(self.v_out, self.i_set, t, load, &mut self.noise);
            let cc = load.is_some_and(|r| r > 0.0 && self.v_out / r >= self.i_set);
            mode = if cc { 2 } else { 1 };
            self.trip_ovp |= v > self.ovp;
            self.trip_ocp |= i > self.ocp;
            if self.trip_ovp || self.trip_ocp {
                self.output_on = false;
                (v, i) = (0.0, 0.0);
            }
        }
        if self.trip_ovp || self.trip_ocp {
            mode = 3;
        }
        self.reading = (((v * 100.0_f64).round() / 100.0).max(0.0), ((i * 1000.0_f64).round() / 1000.0).max(0.0), mode);
        self.reading
    }

    fn dmm_function(&mut self, now: Instant) -> DmmFunction {
        if let Some((f, at)) = self.func_switch
            && now >= at
        {
            self.func = f;
            self.func_switch = None;
        }
        self.func
    }
}

/// `VOLT:DC`, `RES`, … as the SPM names its functions.
fn spm_func_name(f: DmmFunction) -> &'static str {
    f.scpi_func_spm().map_or("ERR", |c| c.trim_start_matches("FUNC:"))
}

/// `CONF:ALL?` answer in the real format: `VOLT:DC,+3.3012V,AUTO,20V`,
/// `RES,+4.7003k Ohm,AUTO,20k Ohm`, `CONT,OL,AUTO,200 Ohm`.
fn spm_conf_all(f: DmmFunction, value: f64) -> String {
    let (unit, ranges): (&str, &[f64]) = match f {
        DmmFunction::VoltDc | DmmFunction::VoltAc | DmmFunction::Diode => ("V", &[0.2, 2.0, 20.0, 200.0, 1000.0]),
        DmmFunction::CurrDc | DmmFunction::CurrAc => ("A", &[0.2, 10.0]),
        DmmFunction::Res | DmmFunction::Cont => (" Ohm", &[200.0, 2e3, 20e3, 200e3, 2e6, 20e6]),
        _ => ("F", &[2e-9, 20e-9, 200e-9, 2e-6, 20e-6, 200e-6, 2e-3]),
    };
    let range = ranges.iter().copied().find(|r| value.abs() < *r).unwrap_or(ranges[ranges.len() - 1]);
    let with_prefix = |x: f64, digits: bool| {
        let (m, p) = format::scale(x);
        if digits { format!("{m:+.4}{p}{unit}") } else { format!("{m}{p}{unit}") }
    };
    let shown = if value.is_nan() { "OL".to_string() } else { with_prefix(value, true) };
    format!("{},{shown},AUTO,{}", spm_func_name(f), with_prefix(range, false))
}

/// A connection to the simulated SPM; implements the port the driver uses.
pub struct FakeSpm {
    dev: SharedSpm,
    line: Vec<u8>,
    out: VecDeque<u8>,
}

impl FakeSpm {
    pub fn new(dev: SharedSpm) -> Self {
        Self { dev, line: Vec::new(), out: VecDeque::new() }
    }
}

impl Read for FakeSpm {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        {
            let mut d = self.dev.lock().unwrap();
            let now = Instant::now();
            while d.replies.front().is_some_and(|(at, _)| *at <= now) {
                if let Some((_, bytes)) = d.replies.pop_front() {
                    self.out.extend(bytes);
                }
            }
        }
        if self.out.is_empty() {
            std::thread::sleep(Duration::from_millis(1));
            return Err(io::ErrorKind::TimedOut.into());
        }
        let n = buf.len().min(self.out.len());
        for (b, x) in buf.iter_mut().zip(self.out.drain(..n)) {
            *b = x;
        }
        Ok(n)
    }
}

impl Write for FakeSpm {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        for &b in buf {
            if b == b'\n' {
                let line = String::from_utf8_lossy(&self.line).trim().to_string();
                self.line.clear();
                self.dev.lock().unwrap().command(&line);
            } else {
                self.line.push(b);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl ScpiPort for FakeSpm {
    /// Like a real port: only what has already arrived is thrown away.
    fn clear_input(&mut self) {
        self.out.clear();
        let mut d = self.dev.lock().unwrap();
        let now = Instant::now();
        d.replies.retain(|(at, _)| *at > now);
    }
}

/// `PowerSourceKind::SpmSimulator`: the real SPM driver on a fake SPM. The
/// simulated supply lives as long as the app, so reconnecting (which many
/// settings do) finds it as it was left – like a real one, it never switches
/// its output back on by itself.
pub fn run_spm(
    cfg: SpmConfig,
    store: Shared,
    ctx: egui::Context,
    psu_rx: Receiver<PsuCommand>,
    dmm_rx: Receiver<DmmCommand>,
    stop: Arc<AtomicBool>,
) {
    static DEVICE: OnceLock<SharedSpm> = OnceLock::new();
    let dev = DEVICE.get_or_init(SpmDevice::shared).clone();
    owon_spm::run_with(cfg, store, ctx, psu_rx, dmm_rx, stop, move |_baud| Ok(FakeSpm::new(dev.clone())));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::ScpiLink;

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

    #[test]
    fn fake_spm_speaks_like_the_real_one() {
        let dev = SpmDevice::shared();
        let mut link = ScpiLink::new(FakeSpm::new(dev.clone()));
        assert_eq!(link.query("*IDN?").unwrap(), SPM_IDN);
        let info = link.query("MEAS:ALL:INFO?").unwrap();
        let a = owon_spm::parse_all_info(&info).unwrap();
        assert_eq!(info.split(',').count(), 7);
        assert!((a.v * 100.0 - (a.v * 100.0).round()).abs() < 1e-9, "10 mV steps: {info}");
        // readings only refresh every 0.3 s
        assert_eq!(link.query("MEAS:ALL:INFO?").unwrap(), info);
        // sets are silent, bad ones answer ERR, chains run the first command only
        assert!(link.send_checked("VOLT 12.000;CURR 2.000").unwrap());
        assert!(!link.send_checked("VOLT 99").unwrap());
        assert!(!link.send_checked("*RST").unwrap());
        assert_eq!(link.query("VOLT?").unwrap(), "12.000");
        assert_eq!(link.query("CURR?").unwrap(), "1.000");
        assert!(owon_spm::parse_dmm_conf_all(&link.query("CONF:ALL?").unwrap()).is_some());
        assert_eq!(link.query("FUNC?").unwrap(), "VOLT:DC");
        // too fast: the second command is lost
        let mut d = dev.lock().unwrap();
        d.replies.clear();
        d.last_cmd = None;
        d.command("VOLT?");
        d.command("CURR?");
        assert_eq!(d.replies.len(), 1);
    }

    #[test]
    fn fake_spm_ramps_and_trips() {
        let dev = SpmDevice::shared();
        let mut d = dev.lock().unwrap();
        let t0 = Instant::now();
        d.execute("OUTP OFF", t0);
        assert_eq!(d.reading(t0).2, 0);
        d.execute("VOLT 11.000", t0);
        d.execute("OUTP ON", t0);
        d.step(t0 + Duration::from_secs(1));
        assert!((d.v_out - 5.5).abs() < 0.05, "{}", d.v_out);
        d.execute("VOLT:LIM 3.000", t0);
        d.reading_at = None;
        let (_, _, mode) = d.reading(t0 + Duration::from_secs(1));
        assert_eq!(mode, 3);
        assert!(d.trip_ovp && !d.output_on);
        assert_eq!(spm_conf_all(DmmFunction::Res, f64::NAN), "RES,OL,AUTO,20M Ohm");
        assert_eq!(spm_conf_all(DmmFunction::VoltDc, 0.0006), "VOLT:DC,+600.0000µV,AUTO,200mV");
    }
}
