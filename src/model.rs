//! Shared measurement store and the live analysis (energy, CV/CC, dropouts).
//!
//! Device threads push samples into the [`Store`]; the UI and the OBS overlay
//! server read from it. Everything lives behind one mutex: pushes are tiny and
//! readers only hold the lock long enough to copy what they need.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Two hours at 100 Hz.
pub const MAX_POWER_SAMPLES: usize = 720_000;
pub const MAX_DMM_SAMPLES: usize = 360_000;
const MAX_EVENTS: usize = 1_000;

pub type Shared = Arc<Mutex<Store>>;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct PowerSample {
    pub t: f64,
    pub v: f64,
    pub i: f64,
}

impl PowerSample {
    pub fn p(&self) -> f64 {
        self.v * self.i
    }
}

/// `value` is NaN when the meter reports overload (OL).
#[derive(Clone, Copy, Debug, Serialize)]
pub struct DmmSample {
    pub t: f64,
    pub value: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum ConnState {
    #[default]
    Disconnected,
    Connecting,
    Connected(String),
    Error(String),
}

impl ConnState {
    pub fn is_connected(&self) -> bool {
        matches!(self, ConnState::Connected(_))
    }
}

/// Regulation mode of the bench supply as inferred from the in-line meter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum RegMode {
    #[default]
    Unknown,
    Off,
    Cv,
    Cc,
}

impl RegMode {
    pub fn label(self) -> &'static str {
        match self {
            RegMode::Unknown => "--",
            RegMode::Off => "OFF",
            RegMode::Cv => "CV",
            RegMode::Cc => "CC",
        }
    }
}

/// Operating mode as reported by a controllable supply (OWON SPM).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum PsuMode {
    #[default]
    Unknown,
    /// Output off.
    Standby,
    Cv,
    Cc,
    Fault,
}

impl PsuMode {
    pub fn label(self) -> &'static str {
        match self {
            PsuMode::Unknown => "--",
            PsuMode::Standby => "AUS",
            PsuMode::Cv => "CV",
            PsuMode::Cc => "CC",
            PsuMode::Fault => "FEHLER",
        }
    }
}

/// What a controllable supply (OWON SPM) last reported about itself.
/// `None` fields haven't been read (yet) or the supply doesn't answer them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PsuState {
    /// e.g. "OWON SPM6103"
    pub model: String,
    pub v_set: Option<f64>,
    pub i_set: Option<f64>,
    /// Over-voltage / over-current protection thresholds.
    pub ovp: Option<f64>,
    pub ocp: Option<f64>,
    pub output_on: Option<bool>,
    pub mode: PsuMode,
    pub trip_ovp: bool,
    pub trip_ocp: bool,
    pub trip_otp: bool,
    /// Largest values the model accepts.
    pub v_max: f64,
    pub i_max: f64,
    /// Store time when mode and trips were last read. `None` if the supply
    /// doesn't report them.
    pub updated: Option<f64>,
    /// Store time when `output_on` was last read from the supply.
    pub output_read: Option<f64>,
    /// Last complaint from the supply, e.g. a rejected command.
    pub note: Option<String>,
}

impl PsuState {
    pub fn fault(&self) -> bool {
        self.mode == PsuMode::Fault
    }

    /// A protection has switched the output off (OVP, OCP, OTP or fault).
    pub fn protection(&self) -> bool {
        self.trip_ovp || self.trip_ocp || self.trip_otp || self.fault()
    }

    /// "OVP", "OCP · OTP", "Fehler" … or `None` when all is well.
    pub fn protection_text(&self) -> Option<String> {
        let mut parts: Vec<&str> = [(self.trip_ovp, "OVP"), (self.trip_ocp, "OCP"), (self.trip_otp, "Übertemperatur")]
            .into_iter()
            .filter_map(|(on, name)| on.then_some(name))
            .collect();
        if parts.is_empty() && self.fault() {
            parts.push("Fehler");
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DmmFunction {
    #[default]
    VoltDc,
    VoltAc,
    CurrDc,
    CurrAc,
    Res,
    Cont,
    Diode,
    Cap,
    Freq,
    Period,
    Temp,
    Unknown,
}

impl DmmFunction {
    /// What the multimeter in an OWON SPM can measure (no frequency,
    /// period or temperature).
    pub const SPM_SELECTABLE: [DmmFunction; 8] = [
        DmmFunction::VoltDc,
        DmmFunction::VoltAc,
        DmmFunction::CurrDc,
        DmmFunction::CurrAc,
        DmmFunction::Res,
        DmmFunction::Cont,
        DmmFunction::Diode,
        DmmFunction::Cap,
    ];

    pub const SELECTABLE: [DmmFunction; 11] = [
        DmmFunction::VoltDc,
        DmmFunction::VoltAc,
        DmmFunction::CurrDc,
        DmmFunction::CurrAc,
        DmmFunction::Res,
        DmmFunction::Cont,
        DmmFunction::Diode,
        DmmFunction::Cap,
        DmmFunction::Freq,
        DmmFunction::Period,
        DmmFunction::Temp,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DmmFunction::VoltDc => "V DC",
            DmmFunction::VoltAc => "V AC",
            DmmFunction::CurrDc => "A DC",
            DmmFunction::CurrAc => "A AC",
            DmmFunction::Res => "Widerstand",
            DmmFunction::Cont => "Durchgang",
            DmmFunction::Diode => "Diode",
            DmmFunction::Cap => "Kapazität",
            DmmFunction::Freq => "Frequenz",
            DmmFunction::Period => "Periode",
            DmmFunction::Temp => "Temperatur",
            DmmFunction::Unknown => "?",
        }
    }

    pub fn unit(self) -> &'static str {
        match self {
            DmmFunction::VoltDc | DmmFunction::VoltAc | DmmFunction::Diode => "V",
            DmmFunction::CurrDc | DmmFunction::CurrAc => "A",
            DmmFunction::Res | DmmFunction::Cont => "Ω",
            DmmFunction::Cap => "F",
            DmmFunction::Freq => "Hz",
            DmmFunction::Period => "s",
            DmmFunction::Temp => "°C",
            DmmFunction::Unknown => "",
        }
    }

    /// Short tag shown next to the value ("DC", "AC", ...).
    pub fn tag(self) -> &'static str {
        match self {
            DmmFunction::VoltDc | DmmFunction::CurrDc => "DC",
            DmmFunction::VoltAc | DmmFunction::CurrAc => "AC",
            DmmFunction::Cont => "Durchgang",
            DmmFunction::Diode => "Diode",
            _ => "",
        }
    }

    /// SCPI command that selects this function on an OWON XDM.
    pub fn scpi_conf(self) -> Option<&'static str> {
        Some(match self {
            DmmFunction::VoltDc => "CONF:VOLT:DC AUTO",
            DmmFunction::VoltAc => "CONF:VOLT:AC AUTO",
            DmmFunction::CurrDc => "CONF:CURR:DC AUTO",
            DmmFunction::CurrAc => "CONF:CURR:AC AUTO",
            DmmFunction::Res => "CONF:RES AUTO",
            DmmFunction::Cont => "CONF:CONT",
            DmmFunction::Diode => "CONF:DIOD",
            DmmFunction::Cap => "CONF:CAP AUTO",
            DmmFunction::Freq => "CONF:FREQ",
            DmmFunction::Period => "CONF:PER",
            DmmFunction::Temp => "CONF:TEMP:RTD PT100",
            DmmFunction::Unknown => return None,
        })
    }

    /// SCPI command that selects this function on the multimeter in an
    /// OWON SPM.
    pub fn scpi_func_spm(self) -> Option<&'static str> {
        Some(match self {
            DmmFunction::VoltDc => "FUNC:VOLT:DC",
            DmmFunction::VoltAc => "FUNC:VOLT:AC",
            DmmFunction::CurrDc => "FUNC:CURR:DC",
            DmmFunction::CurrAc => "FUNC:CURR:AC",
            DmmFunction::Res => "FUNC:RES",
            DmmFunction::Cont => "FUNC:CONT",
            DmmFunction::Diode => "FUNC:DIOD",
            DmmFunction::Cap => "FUNC:CAP",
            DmmFunction::Freq | DmmFunction::Period | DmmFunction::Temp | DmmFunction::Unknown => return None,
        })
    }

    /// Parses the answer of `FUNC?`, e.g. `VOLT`, `"VOLT AC"`, `CURR:AC`.
    pub fn from_scpi(answer: &str) -> DmmFunction {
        let a = answer.trim().trim_matches('"').to_ascii_uppercase();
        let ac = a.contains("AC");
        if a.starts_with("VOLT") {
            if ac { DmmFunction::VoltAc } else { DmmFunction::VoltDc }
        } else if a.starts_with("CURR") {
            if ac { DmmFunction::CurrAc } else { DmmFunction::CurrDc }
        } else if a.starts_with("FRES") || a.starts_with("RES") {
            DmmFunction::Res
        } else if a.starts_with("CONT") {
            DmmFunction::Cont
        } else if a.starts_with("DIOD") {
            DmmFunction::Diode
        } else if a.starts_with("CAP") {
            DmmFunction::Cap
        } else if a.starts_with("FREQ") {
            DmmFunction::Freq
        } else if a.starts_with("PER") {
            DmmFunction::Period
        } else if a.starts_with("TEMP") {
            DmmFunction::Temp
        } else {
            DmmFunction::Unknown
        }
    }
}

/// User-tunable analysis parameters. Persisted with the app settings.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AnalysisSettings {
    /// Voltage set on the supply (the in-line meter can't read the knob).
    /// A supply that reports its own set points (OWON SPM) wins over this.
    pub v_set: Option<f64>,
    /// Current limit set on the supply.
    pub i_set: Option<f64>,
    /// Learn `v_set` automatically while the output is unloaded.
    pub auto_learn_vset: bool,
    /// Learn `i_set` automatically from a short circuit / hard current limit.
    pub auto_learn_iset: bool,
    /// A dip of more than this many percent below the reference is a dropout.
    pub dropout_pct: f64,
    /// Voltage within this many percent of the set point counts as CV.
    pub cv_tol_pct: f64,
    /// Current within this many percent of the limit counts as CC.
    pub cc_tol_pct: f64,
    /// Below this output voltage with current flowing we call it a short.
    pub short_volts: f64,
    /// Averaging window for the big numbers (graphs always show raw data).
    pub display_avg_ms: f64,
}

impl Default for AnalysisSettings {
    fn default() -> Self {
        Self {
            v_set: None,
            i_set: None,
            auto_learn_vset: true,
            auto_learn_iset: true,
            dropout_pct: 5.0,
            cv_tol_pct: 1.5,
            cc_tol_pct: 2.0,
            short_volts: 0.3,
            display_avg_ms: 100.0,
        }
    }
}

/// Correction for the in-line meter, typically found by comparing against
/// the multimeter (see "Mit Multimeter kalibrieren" in the app).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Calibration {
    pub v_gain: f64,
    pub i_gain: f64,
    /// Current read with nothing connected, subtracted before the gain.
    pub i_offset: f64,
}

impl Default for Calibration {
    fn default() -> Self {
        Self { v_gain: 1.0, i_gain: 1.0, i_offset: 0.0 }
    }
}

impl Calibration {
    pub fn apply(&self, s: PowerSample) -> PowerSample {
        PowerSample { t: s.t, v: s.v * self.v_gain, i: (s.i - self.i_offset) * self.i_gain }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum EventKind {
    Dropout,
    CurrentLimit,
    Short,
    /// The supply switched itself off (OVP/OCP/OTP/fault), as it reported.
    Protection,
    Marker,
}

impl EventKind {
    pub fn label(self) -> &'static str {
        match self {
            EventKind::Dropout => "Spannungseinbruch",
            EventKind::CurrentLimit => "Strombegrenzung (CC)",
            EventKind::Short => "Kurzschluss",
            EventKind::Protection => "Schutzabschaltung",
            EventKind::Marker => "Marker",
        }
    }
}

/// Output slew rate assumed for the ramp after a set point change or
/// switching on (the SPM does about 5.5 V/s).
const RAMP_V_PER_S: f64 = 5.0;
/// Device mode older than this is not trusted any more.
const DEVICE_FRESH_S: f64 = 1.5;

#[derive(Clone, Debug, Serialize)]
pub struct PowerEvent {
    pub kind: EventKind,
    pub t_start: f64,
    pub t_end: Option<f64>,
    /// Lowest voltage seen during the event.
    pub v_min: f64,
    /// Highest current seen during the event.
    pub i_max: f64,
}

impl PowerEvent {
    pub fn duration(&self, now: f64) -> f64 {
        self.t_end.unwrap_or(now) - self.t_start
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct MinMax {
    pub min: f64,
    pub max: f64,
    pub sum: f64,
    pub n: u64,
}

impl MinMax {
    pub fn add(&mut self, x: f64) {
        if !x.is_finite() {
            return;
        }
        if self.n == 0 {
            self.min = x;
            self.max = x;
        } else {
            self.min = self.min.min(x);
            self.max = self.max.max(x);
        }
        self.sum += x;
        self.n += 1;
    }

    pub fn avg(&self) -> Option<f64> {
        (self.n > 0).then(|| self.sum / self.n as f64)
    }
}

/// Streaming analysis of the supply output.
#[derive(Clone, Debug, Default)]
pub struct PowerAnalyzer {
    pub energy_wh: f64,
    pub charge_ah: f64,
    pub v: MinMax,
    pub i: MinMax,
    pub p: MinMax,
    pub mode: RegMode,
    pub learned_vset: Option<f64>,
    pub learned_iset: Option<f64>,
    pub events: VecDeque<PowerEvent>,
    last_t: Option<f64>,
    last: Option<PowerSample>,
    /// Last state reported by a controllable supply.
    device: Option<PsuState>,
    /// While the output ramps to a new set point no dropout is detected;
    /// ends when the voltage reaches the CV band or at this time.
    ramp_until: Option<f64>,
    /// Slow average of the voltage, used as dropout reference when no set
    /// point is known.
    v_ema: Option<f64>,
    /// Slow average of the current outside of events.
    i_ema: Option<f64>,
    /// Current statistics during an open dip: (sum, sum², n, start).
    dip_i: (f64, f64, u32, f64),
    pending_mode: RegMode,
    pending_since: f64,
    no_load_since: Option<f64>,
    no_load_acc: (f64, u32),
    open_dropout: Option<usize>,
    open_cc: Option<usize>,
    open_short: Option<usize>,
    open_protection: Option<usize>,
}

impl PowerAnalyzer {
    pub fn reset_stats(&mut self) {
        self.energy_wh = 0.0;
        self.charge_ah = 0.0;
        self.v = MinMax::default();
        self.i = MinMax::default();
        self.p = MinMax::default();
    }

    /// Set points read from the supply itself.
    pub fn device_vset(&self) -> Option<f64> {
        self.device.as_ref().and_then(|d| d.v_set)
    }

    pub fn device_iset(&self) -> Option<f64> {
        self.device.as_ref().and_then(|d| d.i_set)
    }

    // Set point precedence: supply > typed in by the user > learned.

    pub fn v_ref(&self, s: &AnalysisSettings) -> Option<f64> {
        self.v_set_effective(s).or(self.v_ema)
    }

    pub fn i_limit(&self, s: &AnalysisSettings) -> Option<f64> {
        self.device_iset().or(s.i_set).or(self.learned_iset)
    }

    pub fn v_set_effective(&self, s: &AnalysisSettings) -> Option<f64> {
        self.device_vset().or(s.v_set).or(self.learned_vset)
    }

    /// True when the voltage set point shown is only a learned guess.
    pub fn v_set_is_learned(&self, s: &AnalysisSettings) -> bool {
        self.device_vset().is_none() && s.v_set.is_none() && self.learned_vset.is_some()
    }

    pub fn i_set_is_learned(&self, s: &AnalysisSettings) -> bool {
        self.device_iset().is_none() && s.i_set.is_none() && self.learned_iset.is_some()
    }

    pub fn active(&self, kind: EventKind) -> bool {
        let idx = match kind {
            EventKind::Dropout => self.open_dropout,
            EventKind::CurrentLimit => self.open_cc,
            EventKind::Short => self.open_short,
            EventKind::Protection => self.open_protection,
            EventKind::Marker => None,
        };
        idx.is_some()
    }

    /// New state from a controllable supply. Starts the ramp window when
    /// the output was switched on or the set point changed, and opens or
    /// closes the protection event.
    pub fn on_device(&mut self, dev: &PsuState, now: f64) {
        let prev = self.device.take();
        let prev_vset = prev.as_ref().and_then(|p| p.v_set);
        let switched_on = dev.output_on == Some(true) && prev.as_ref().is_some_and(|p| p.output_on == Some(false));
        let new_vset = matches!((prev_vset, dev.v_set), (Some(a), Some(b)) if (a - b).abs() > 0.0005);
        if new_vset {
            // The voltage moved because someone turned the knob, not
            // because of a bad contact.
            self.discard_open_dropout();
        }
        if (switched_on || new_vset)
            && let Some(target) = dev.v_set
        {
            let from = if switched_on { 0.0 } else { self.last.map_or(0.0, |s| s.v) };
            self.ramp_until = Some(now + (target - from).abs() / RAMP_V_PER_S + 1.0);
        }
        let at = PowerSample { t: now, ..self.last.unwrap_or(PowerSample { t: now, v: f64::NAN, i: f64::NAN }) };
        self.track(EventKind::Protection, dev.protection(), &at);
        self.device = Some(dev.clone());
    }

    /// The supply went away: its set points no longer count. Its last set
    /// points become the learned ones – learning was paused while it was
    /// connected, so anything learned before would be stale.
    pub fn on_device_lost(&mut self, now: f64) {
        if let Some(dev) = self.device.take() {
            if dev.v_set.is_some() {
                self.learned_vset = dev.v_set;
            }
            if dev.i_set.is_some() {
                self.learned_iset = dev.i_set;
            }
            let at = PowerSample { t: now, ..self.last.unwrap_or(PowerSample { t: now, v: f64::NAN, i: f64::NAN }) };
            self.track(EventKind::Protection, false, &at);
        }
        self.ramp_until = None;
        self.no_load_since = None;
        self.no_load_acc = (0.0, 0);
        self.dip_i = (0.0, 0.0, 0, 0.0);
    }

    /// Regulation mode as reported by the supply, if it reports one and
    /// the report is recent.
    fn device_mode(&self, t: f64) -> Option<RegMode> {
        let d = self.device.as_ref()?;
        let fresh = |at: Option<f64>| at.is_some_and(|u| t - u < DEVICE_FRESH_S);
        if fresh(d.updated) {
            match d.mode {
                PsuMode::Cv => return Some(RegMode::Cv),
                PsuMode::Cc => return Some(RegMode::Cc),
                PsuMode::Standby | PsuMode::Fault => return Some(RegMode::Off),
                PsuMode::Unknown => {}
            }
        }
        // An old "output off" may have been switched on at the front panel.
        (d.output_on == Some(false) && fresh(d.output_read)).then_some(RegMode::Off)
    }

    fn discard_open_dropout(&mut self) {
        if let Some(idx) = self.open_dropout.take()
            && idx < self.events.len()
        {
            self.events.remove(idx);
            for open in [&mut self.open_cc, &mut self.open_short, &mut self.open_protection] {
                if let Some(i) = open.as_mut()
                    && *i > idx
                {
                    *i -= 1;
                }
            }
        }
        self.dip_i = (0.0, 0.0, 0, 0.0);
    }

    pub fn push(&mut self, s: &PowerSample, cfg: &AnalysisSettings) {
        let p = s.p();
        let dt = self.last_t.map(|last| (s.t - last).clamp(0.0, 1.0));
        if let Some(dt) = dt {
            self.energy_wh += p * dt / 3600.0;
            self.charge_ah += s.i * dt / 3600.0;
        }
        self.last_t = Some(s.t);
        self.last = Some(*s);
        self.v.add(s.v);
        self.i.add(s.i);
        self.p.add(p);

        self.learn(s, cfg);

        let v_ref = self.v_ref(cfg);
        let i_lim = self.i_limit(cfg);

        // --- regulation mode -------------------------------------------------
        let device_mode = self.device_mode(s.t);
        let sample_cc = i_lim.is_some_and(|lim| lim > 0.0 && s.i >= lim * (1.0 - cfg.cc_tol_pct / 100.0));
        let raw = if let Some(m) = device_mode {
            // The supply reports CC up to ~0.4 s late; samples from the box
            // at the limit already show it. They win over a reported CV.
            if m == RegMode::Cv && sample_cc { RegMode::Cc } else { m }
        } else if s.v < 0.05 && s.i.abs() < 0.002 {
            RegMode::Off
        } else if sample_cc {
            RegMode::Cc
        } else if let Some(vr) = v_ref.filter(|v| *v > 0.1) {
            if s.v >= vr * (1.0 - cfg.cv_tol_pct / 100.0) {
                RegMode::Cv
            } else if i_lim.is_none() {
                RegMode::Unknown
            } else {
                // Below the set voltage but not at the limit: transient sag.
                RegMode::Cv
            }
        } else {
            RegMode::Unknown
        };
        if raw != self.pending_mode {
            self.pending_mode = raw;
            self.pending_since = s.t;
        }
        // 20 ms hysteresis keeps the badge from flickering on noise. The
        // supply's own report needs none.
        if device_mode.is_some() || (self.mode != self.pending_mode && s.t - self.pending_since >= 0.02) {
            self.mode = self.pending_mode;
        }

        // --- ramp after switching on / a new set point ------------------------
        let in_ramp = match (self.ramp_until, v_ref) {
            (Some(until), Some(vr)) => {
                let settled = (s.v - vr).abs() <= vr * cfg.cv_tol_pct / 100.0;
                if settled || s.t >= until {
                    self.ramp_until = None;
                }
                self.ramp_until.is_some()
            }
            (Some(_), None) => true,
            (None, _) => false,
        };

        // --- events ----------------------------------------------------------
        let is_short = s.v < cfg.short_volts && s.i > 0.01 && v_ref.is_some_and(|vr| vr > cfg.short_volts * 3.0);
        let is_cc = raw == RegMode::Cc && !is_short;
        let is_dropout = !is_short
            && !is_cc
            && !in_ramp
            && v_ref.is_some_and(|vr| vr > 0.5 && s.v < vr * (1.0 - cfg.dropout_pct / 100.0))
            && raw != RegMode::Off;

        self.track(EventKind::Short, is_short, s);
        self.track(EventKind::CurrentLimit, is_cc, s);
        self.track(EventKind::Dropout, is_dropout, s);
        self.learn_limit_from_plateau(is_dropout, s, cfg);

        // Reference follows the voltage slowly, but not during a dip, so the
        // dip itself never becomes the new normal.
        if !is_dropout && !is_short && !is_cc && s.v > 0.05 {
            // ~5 s time constant at any sample rate (0.002 at 100 Hz)
            let alpha = dt.map_or(1.0, |dt| (dt / 5.0).min(1.0));
            self.v_ema = Some(match self.v_ema {
                Some(e) => e + alpha * (s.v - e),
                None => s.v,
            });
            self.i_ema = Some(match self.i_ema {
                Some(e) => e + alpha * (s.i - e),
                None => s.i,
            });
        }
    }

    /// A dip during which the current sits dead flat, above what the load
    /// drew before, is the supply hitting its current limit – not a bad
    /// contact. Learn the limit and relabel the event as CC.
    fn learn_limit_from_plateau(&mut self, is_dropout: bool, s: &PowerSample, cfg: &AnalysisSettings) {
        if !is_dropout || !cfg.auto_learn_iset || cfg.i_set.is_some() || self.device_iset().is_some() {
            self.dip_i = (0.0, 0.0, 0, 0.0);
            return;
        }
        let d = &mut self.dip_i;
        if d.2 == 0 {
            d.3 = s.t;
        }
        d.0 += s.i;
        d.1 += s.i * s.i;
        d.2 += 1;
        let n = d.2 as f64;
        let mean = d.0 / n;
        let std = (d.1 / n - mean * mean).max(0.0).sqrt();
        // At least 3 samples over 150 ms, so it works at 10 Hz as well.
        let flat = d.2 >= 3 && s.t - d.3 >= 0.15 && mean > 0.01 && std < mean * 0.005;
        if flat && self.i_ema.is_none_or(|e| mean > e * 1.02) {
            self.learned_iset = Some(mean);
            if let Some(idx) = self.open_dropout.take() {
                if let Some(ev) = self.events.get_mut(idx) {
                    ev.kind = EventKind::CurrentLimit;
                }
                self.open_cc = Some(idx);
            }
            self.dip_i = (0.0, 0.0, 0, 0.0);
        }
    }

    fn learn(&mut self, s: &PowerSample, cfg: &AnalysisSettings) {
        // Nothing to guess when the supply tells us.
        // Unloaded output sits exactly at the set voltage.
        if cfg.auto_learn_vset && self.device_vset().is_none() && s.i.abs() < 0.003 && s.v > 0.3 {
            let since = *self.no_load_since.get_or_insert(s.t);
            self.no_load_acc.0 += s.v;
            self.no_load_acc.1 += 1;
            if s.t - since >= 0.3 {
                let avg = self.no_load_acc.0 / self.no_load_acc.1 as f64;
                self.learned_vset = Some(avg);
            }
        } else {
            self.no_load_since = None;
            self.no_load_acc = (0.0, 0);
        }
        // During a hard short the supply delivers exactly its current limit.
        if cfg.auto_learn_iset && self.device_iset().is_none() && s.v < cfg.short_volts && s.i > 0.01 {
            self.learned_iset = Some(match self.learned_iset {
                Some(l) => l + 0.1 * (s.i - l),
                None => s.i,
            });
        }
    }

    fn track(&mut self, kind: EventKind, active: bool, s: &PowerSample) {
        let slot = match kind {
            EventKind::Dropout => &mut self.open_dropout,
            EventKind::CurrentLimit => &mut self.open_cc,
            EventKind::Short => &mut self.open_short,
            EventKind::Protection => &mut self.open_protection,
            EventKind::Marker => return,
        };
        match (*slot, active) {
            (None, true) => {
                self.events.push_back(PowerEvent { kind, t_start: s.t, t_end: None, v_min: s.v, i_max: s.i });
                *slot = Some(self.events.len() - 1);
            }
            (Some(idx), true) => {
                if let Some(ev) = self.events.get_mut(idx) {
                    ev.v_min = ev.v_min.min(s.v);
                    ev.i_max = ev.i_max.max(s.i);
                }
            }
            (Some(idx), false) => {
                if let Some(ev) = self.events.get_mut(idx) {
                    ev.t_end = Some(s.t);
                }
                *slot = None;
            }
            (None, false) => {}
        }
        if self.events.len() > MAX_EVENTS {
            self.events.pop_front();
            for open in [&mut self.open_dropout, &mut self.open_cc, &mut self.open_short, &mut self.open_protection] {
                *open = open.and_then(|i| i.checked_sub(1));
            }
        }
    }

    pub fn add_marker(&mut self, t: f64) {
        self.events.push_back(PowerEvent {
            kind: EventKind::Marker,
            t_start: t,
            t_end: Some(t),
            v_min: f64::NAN,
            i_max: f64::NAN,
        });
    }
}

#[derive(Default)]
pub struct PowerData {
    /// Whoever delivers the samples (PowerMon box, SPM, simulator).
    pub conn: ConnState,
    /// Control link of a controllable supply (OWON SPM); in the hybrid
    /// setup the samples come from the box while this one stays up.
    pub psu_conn: ConnState,
    pub psu: Option<PsuState>,
    pub samples: VecDeque<PowerSample>,
    pub analyzer: PowerAnalyzer,
    pub seq: u64,
}

#[derive(Default)]
pub struct DmmData {
    pub conn: ConnState,
    pub function: DmmFunction,
    pub samples: VecDeque<DmmSample>,
    pub stats: MinMax,
    pub seq: u64,
}

pub struct Store {
    t0: Instant,
    pub analysis: AnalysisSettings,
    pub calibration: Calibration,
    pub power: PowerData,
    pub dmm: DmmData,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            t0: Instant::now(),
            analysis: AnalysisSettings::default(),
            calibration: Calibration::default(),
            power: PowerData::default(),
            dmm: DmmData::default(),
        }
    }
}

impl Store {
    pub fn shared() -> Shared {
        Arc::new(Mutex::new(Store::default()))
    }

    /// Seconds since the app started; the common time axis of all channels.
    pub fn now(&self) -> f64 {
        self.t0.elapsed().as_secs_f64()
    }

    pub fn t0(&self) -> Instant {
        self.t0
    }

    pub fn push_power(&mut self, v: f64, i: f64) {
        let s = PowerSample { t: self.now(), v, i };
        self.push_power_sample(s);
    }

    /// A sample from the PowerMon box: its calibration is applied here.
    pub fn push_power_sample(&mut self, raw: PowerSample) {
        let s = self.calibration.apply(raw);
        self.push_power_sample_uncalibrated(s);
    }

    /// A sample from a source with its own calibration (OWON SPM).
    pub fn push_power_sample_uncalibrated(&mut self, s: PowerSample) {
        self.power.analyzer.push(&s, &self.analysis);
        self.power.samples.push_back(s);
        if self.power.samples.len() > MAX_POWER_SAMPLES {
            self.power.samples.pop_front();
        }
        self.power.seq += 1;
    }

    pub fn push_dmm(&mut self, value: f64) {
        let s = DmmSample { t: self.now(), value };
        self.dmm.stats.add(value);
        self.dmm.samples.push_back(s);
        if self.dmm.samples.len() > MAX_DMM_SAMPLES {
            self.dmm.samples.pop_front();
        }
        self.dmm.seq += 1;
    }

    pub fn set_dmm_function(&mut self, f: DmmFunction) {
        if self.dmm.function != f {
            self.dmm.function = f;
            // Mixing units in one trace makes no sense.
            self.dmm.samples.clear();
            self.dmm.stats = MinMax::default();
        }
    }

    /// New state read from a controllable supply.
    pub fn set_psu(&mut self, st: PsuState) {
        let now = self.now();
        self.power.analyzer.on_device(&st, now);
        self.power.psu = Some(st);
    }

    pub fn clear_psu(&mut self) {
        let now = self.now();
        self.power.analyzer.on_device_lost(now);
        self.power.psu = None;
    }

    pub fn add_marker(&mut self) {
        let t = self.now();
        self.power.analyzer.add_marker(t);
    }

    pub fn clear_all(&mut self) {
        self.power.samples.clear();
        let old = std::mem::take(&mut self.power.analyzer);
        // The supply's set points, and a ramp it is still in, are still
        // valid after "Leeren".
        self.power.analyzer.device = old.device;
        self.power.analyzer.ramp_until = old.ramp_until;
        self.power.analyzer.last = old.last;
        self.dmm.samples.clear();
        self.dmm.stats = MinMax::default();
    }

    /// Latest reading, averaged over `display_avg_ms` so the last digit is
    /// readable on camera. Returns the raw sample when the window is 0.
    pub fn display_power(&self) -> Option<PowerSample> {
        let last = *self.power.samples.back()?;
        let w = self.analysis.display_avg_ms / 1000.0;
        if w <= 0.0 {
            return Some(last);
        }
        let (mut sv, mut si, mut n) = (0.0, 0.0, 0u32);
        for s in self.power.samples.iter().rev().take_while(|s| last.t - s.t <= w) {
            sv += s.v;
            si += s.i;
            n += 1;
        }
        Some(PowerSample { t: last.t, v: sv / n as f64, i: si / n as f64 })
    }

    /// Mean voltage and current over the last `window` seconds.
    pub fn avg_power(&self, window: f64) -> Option<(f64, f64)> {
        let now = self.power.samples.back()?.t;
        let (mut v, mut i, mut n) = (0.0, 0.0, 0u32);
        for s in self.power.samples.iter().rev().take_while(|s| now - s.t <= window) {
            v += s.v;
            i += s.i;
            n += 1;
        }
        (n > 0).then(|| (v / n as f64, i / n as f64))
    }

    /// Mean multimeter reading over the last `window` seconds (ignores OL).
    pub fn avg_dmm(&self, window: f64) -> Option<f64> {
        let now = self.dmm.samples.back()?.t;
        let mut mm = MinMax::default();
        for s in self.dmm.samples.iter().rev().take_while(|s| now - s.t <= window) {
            mm.add(s.value);
        }
        mm.avg()
    }

    /// Peak-to-peak voltage over the last `window` seconds (ripple/noise).
    pub fn ripple(&self, window: f64) -> Option<f64> {
        let now = self.power.samples.back()?.t;
        let mut mm = MinMax::default();
        for s in self.power.samples.iter().rev().take_while(|s| now - s.t <= window) {
            mm.add(s.v);
        }
        (mm.n > 1).then_some(mm.max - mm.min)
    }
}

/// Index of the first sample with `t >= t_min` (samples are time ordered).
pub fn first_index_after<T>(samples: &VecDeque<T>, t_min: f64, time: impl Fn(&T) -> f64) -> usize {
    samples.partition_point(|s| time(s) < t_min)
}

/// Min/max decimation: keeps spikes and dropouts visible no matter how many
/// samples are squeezed into the available pixels.
pub fn decimate(points: impl ExactSizeIterator<Item = [f64; 2]>, max_points: usize) -> Vec<[f64; 2]> {
    let n = points.len();
    if n <= max_points || max_points < 4 {
        return points.filter(|p| p[1].is_finite()).collect();
    }
    let buckets = max_points / 2;
    let per = n.div_ceil(buckets);
    let mut out = Vec::with_capacity(buckets * 2 + 2);
    let mut chunk: Vec<[f64; 2]> = Vec::with_capacity(per);
    let flush = |chunk: &mut Vec<[f64; 2]>, out: &mut Vec<[f64; 2]>| {
        let finite = chunk.iter().filter(|p| p[1].is_finite());
        let mut lo: Option<[f64; 2]> = None;
        let mut hi: Option<[f64; 2]> = None;
        for p in finite {
            if lo.is_none_or(|l| p[1] < l[1]) {
                lo = Some(*p);
            }
            if hi.is_none_or(|h| p[1] > h[1]) {
                hi = Some(*p);
            }
        }
        if let (Some(lo), Some(hi)) = (lo, hi) {
            if lo[0] <= hi[0] {
                out.push(lo);
                out.push(hi);
            } else {
                out.push(hi);
                out.push(lo);
            }
        }
        chunk.clear();
    };
    for p in points {
        chunk.push(p);
        if chunk.len() == per {
            flush(&mut chunk, &mut out);
        }
    }
    if !chunk.is_empty() {
        flush(&mut chunk, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(a: &mut PowerAnalyzer, cfg: &AnalysisSettings, t0: f64, secs: f64, v: f64, i: f64) -> f64 {
        let mut t = t0;
        while t < t0 + secs {
            a.push(&PowerSample { t, v, i }, cfg);
            t += 0.01;
        }
        t
    }

    #[test]
    fn integrates_energy_and_charge() {
        let cfg = AnalysisSettings::default();
        let mut a = PowerAnalyzer::default();
        feed(&mut a, &cfg, 0.0, 36.0, 10.0, 1.0);
        // 10 W for 36 s = 0.1 Wh, 1 A for 36 s = 10 mAh
        assert!((a.energy_wh - 0.1).abs() < 1e-3, "{}", a.energy_wh);
        assert!((a.charge_ah * 1000.0 - 10.0).abs() < 0.1);
    }

    #[test]
    fn learns_vset_and_detects_cv_cc() {
        let cfg = AnalysisSettings { i_set: Some(1.0), ..Default::default() };
        let mut a = PowerAnalyzer::default();
        let t = feed(&mut a, &cfg, 0.0, 1.0, 19.0, 0.0);
        assert!((a.learned_vset.unwrap() - 19.0).abs() < 1e-9);
        let t = feed(&mut a, &cfg, t, 1.0, 18.99, 0.5);
        assert_eq!(a.mode, RegMode::Cv);
        feed(&mut a, &cfg, t, 1.0, 14.0, 1.0);
        assert_eq!(a.mode, RegMode::Cc);
        assert!(a.active(EventKind::CurrentLimit));
        assert!(!a.active(EventKind::Dropout));
    }

    #[test]
    fn detects_dropout_and_short() {
        let cfg = AnalysisSettings::default();
        let mut a = PowerAnalyzer::default();
        let t = feed(&mut a, &cfg, 0.0, 1.0, 12.0, 0.0);
        let t = feed(&mut a, &cfg, t, 1.0, 12.0, 0.5);
        let t = feed(&mut a, &cfg, t, 0.05, 9.0, 0.4);
        assert!(a.active(EventKind::Dropout));
        let t = feed(&mut a, &cfg, t, 0.5, 12.0, 0.5);
        assert!(!a.active(EventKind::Dropout));
        let dropouts: Vec<_> = a.events.iter().filter(|e| e.kind == EventKind::Dropout).collect();
        assert_eq!(dropouts.len(), 1);
        assert!((dropouts[0].v_min - 9.0).abs() < 1e-9);
        assert!(dropouts[0].t_end.is_some());

        feed(&mut a, &cfg, t, 0.5, 0.02, 2.0);
        assert!(a.active(EventKind::Short));
        assert!((a.learned_iset.unwrap() - 2.0).abs() < 0.01);
    }

    #[test]
    fn learns_current_limit_from_flat_plateau() {
        let cfg = AnalysisSettings::default();
        let mut a = PowerAnalyzer::default();
        let t = feed(&mut a, &cfg, 0.0, 1.0, 19.0, 0.0);
        let t = feed(&mut a, &cfg, t, 2.0, 19.0, 0.5);
        // load grows past the limit: voltage collapses, current pinned at 1 A
        let t = feed(&mut a, &cfg, t, 0.5, 15.0, 1.0);
        assert!((a.learned_iset.unwrap() - 1.0).abs() < 1e-6);
        assert!(a.active(EventKind::CurrentLimit));
        assert!(!a.active(EventKind::Dropout));
        assert_eq!(a.mode, RegMode::Cc);
        feed(&mut a, &cfg, t, 0.5, 19.0, 0.5);
        assert!(a.events.iter().all(|e| e.kind == EventKind::CurrentLimit));
    }

    #[test]
    fn calibration_applies_offset_then_gain() {
        let c = Calibration { v_gain: 1.001, i_gain: 0.99, i_offset: 0.002 };
        let s = c.apply(PowerSample { t: 1.0, v: 10.0, i: 1.002 });
        assert!((s.v - 10.01).abs() < 1e-12);
        assert!((s.i - 0.99).abs() < 1e-12);
    }

    #[test]
    fn decimation_keeps_spikes() {
        let pts: Vec<[f64; 2]> = (0..10_000).map(|k| [k as f64, if k == 5_000 { 100.0 } else { 1.0 }]).collect();
        let d = decimate(pts.into_iter(), 200);
        assert!(d.len() <= 202);
        assert!(d.iter().any(|p| p[1] == 100.0));
        assert!(d.windows(2).all(|w| w[0][0] <= w[1][0]));
    }

    /// Like `feed`, but at any sample rate.
    fn feed_hz(a: &mut PowerAnalyzer, cfg: &AnalysisSettings, t0: f64, secs: f64, hz: f64, v: f64, i: f64) -> f64 {
        let n = (secs * hz).round() as usize;
        for k in 0..n {
            a.push(&PowerSample { t: t0 + k as f64 / hz, v, i }, cfg);
        }
        t0 + n as f64 / hz
    }

    fn spm(v_set: f64, i_set: f64, on: bool, t: f64) -> PsuState {
        PsuState {
            model: "OWON SPM6103".into(),
            v_set: Some(v_set),
            i_set: Some(i_set),
            output_on: Some(on),
            mode: if on { PsuMode::Cv } else { PsuMode::Standby },
            v_max: 60.0,
            i_max: 10.0,
            updated: Some(t),
            ..Default::default()
        }
    }

    #[test]
    fn device_set_points_win_and_stop_learning() {
        let cfg = AnalysisSettings { v_set: Some(19.0), i_set: Some(1.0), ..Default::default() };
        let mut a = PowerAnalyzer::default();
        a.on_device(&spm(12.0, 2.0, true, 0.0), 0.0);
        assert_eq!(a.v_set_effective(&cfg), Some(12.0));
        assert_eq!(a.i_limit(&cfg), Some(2.0));
        assert!(!a.v_set_is_learned(&cfg));
        // unloaded output and a short would normally teach set points
        let t = feed_hz(&mut a, &AnalysisSettings::default(), 0.0, 1.0, 10.0, 12.01, 0.0);
        let mut dev = spm(12.0, 2.0, true, t);
        dev.mode = PsuMode::Cc;
        a.on_device(&dev, t);
        feed_hz(&mut a, &AnalysisSettings::default(), t, 0.5, 10.0, 0.02, 2.0);
        assert_eq!(a.learned_vset, None);
        assert_eq!(a.learned_iset, None);
        assert_eq!(a.mode, RegMode::Cc, "mode comes from the supply");
        // supply gone: manual values count again
        a.on_device_lost(2.0);
        assert_eq!(a.v_set_effective(&cfg), Some(19.0));
    }

    #[test]
    fn stale_device_mode_is_ignored() {
        let cfg = AnalysisSettings::default();
        let mut a = PowerAnalyzer::default();
        let mut dev = spm(12.0, 1.0, true, 0.0);
        dev.mode = PsuMode::Cc;
        a.on_device(&dev, 0.0);
        feed_hz(&mut a, &cfg, 0.0, 0.5, 10.0, 12.0, 0.2);
        assert_eq!(a.mode, RegMode::Cc);
        // 3 s later the report is too old: inferred from the samples again
        feed_hz(&mut a, &cfg, 3.0, 0.5, 10.0, 12.0, 0.2);
        assert_eq!(a.mode, RegMode::Cv);
    }

    #[test]
    fn no_dropout_while_output_ramps_up() {
        let cfg = AnalysisSettings::default();
        let mut a = PowerAnalyzer::default();
        a.on_device(&spm(12.0, 1.0, false, 0.0), 0.0);
        let mut t = feed_hz(&mut a, &cfg, 0.0, 1.0, 10.0, 0.0, 0.0);
        assert_eq!(a.mode, RegMode::Off);
        a.on_device(&spm(12.0, 1.0, true, t), t);
        // 5.5 V/s up to 12 V, at 10 Hz, with the supply reporting CV
        let mut v: f64 = 0.0;
        while v < 12.0 {
            v = (v + 0.55).min(12.0);
            a.push(&PowerSample { t, v, i: v / 50.0 }, &cfg);
            a.on_device(&spm(12.0, 1.0, true, t), t);
            t += 0.1;
        }
        feed_hz(&mut a, &cfg, t, 1.0, 10.0, 12.0, 0.24);
        assert!(a.events.iter().all(|e| e.kind != EventKind::Dropout), "{:?}", a.events);

        // turning the knob down while a "dip" is open discards that dip
        let t = feed_hz(&mut a, &cfg, t + 1.0, 0.3, 10.0, 9.0, 0.18);
        assert!(a.active(EventKind::Dropout));
        a.on_device(&spm(9.0, 1.0, true, t), t);
        assert!(!a.active(EventKind::Dropout));
        assert!(a.events.iter().all(|e| e.kind != EventKind::Dropout));
    }

    #[test]
    fn detects_real_dropout_at_10_hz() {
        let cfg = AnalysisSettings::default();
        let mut a = PowerAnalyzer::default();
        a.on_device(&spm(12.0, 3.0, true, 0.0), 0.0);
        let mut t = 0.0;
        for _ in 0..20 {
            a.on_device(&spm(12.0, 3.0, true, t), t);
            t = feed_hz(&mut a, &cfg, t, 0.1, 10.0, 12.0, 0.5);
        }
        // 300 ms sag with less current: a bad contact, not the limit
        for _ in 0..3 {
            a.on_device(&spm(12.0, 3.0, true, t), t);
            t = feed_hz(&mut a, &cfg, t, 0.1, 10.0, 9.0, 0.4);
        }
        assert!(a.active(EventKind::Dropout));
        a.on_device(&spm(12.0, 3.0, true, t), t);
        feed_hz(&mut a, &cfg, t, 0.5, 10.0, 12.0, 0.5);
        let dips: Vec<_> = a.events.iter().filter(|e| e.kind == EventKind::Dropout).collect();
        assert_eq!(dips.len(), 1);
        assert!((dips[0].v_min - 9.0).abs() < 1e-9);
    }

    #[test]
    fn learns_limit_from_plateau_at_10_hz() {
        let cfg = AnalysisSettings::default();
        let mut a = PowerAnalyzer::default();
        let t = feed_hz(&mut a, &cfg, 0.0, 1.0, 10.0, 19.0, 0.0);
        let t = feed_hz(&mut a, &cfg, t, 2.0, 10.0, 19.0, 0.5);
        feed_hz(&mut a, &cfg, t, 0.5, 10.0, 15.0, 1.0);
        assert!((a.learned_iset.unwrap() - 1.0).abs() < 1e-6);
        assert!(a.active(EventKind::CurrentLimit));
    }

    #[test]
    fn ema_is_independent_of_sample_rate() {
        let cfg = AnalysisSettings::default();
        let mut slow = PowerAnalyzer::default();
        let mut fast = PowerAnalyzer::default();
        // voltage creeps up 1 % (inside the dropout band) for 5 s
        let t = feed_hz(&mut slow, &cfg, 0.0, 1.0, 10.0, 10.0, 0.5);
        feed_hz(&mut slow, &cfg, t, 5.0, 10.0, 10.1, 0.5);
        let t = feed_hz(&mut fast, &cfg, 0.0, 1.0, 100.0, 10.0, 0.5);
        feed_hz(&mut fast, &cfg, t, 5.0, 100.0, 10.1, 0.5);
        let (es, ef) = (slow.v_ema.unwrap(), fast.v_ema.unwrap());
        // 1 - e^-1 of the step after one time constant, at both rates
        assert!((ef - 10.063).abs() < 0.002, "{ef}");
        assert!((es - ef).abs() < 0.002, "{es} vs {ef}");
    }

    #[test]
    fn protection_event_opens_and_closes() {
        let cfg = AnalysisSettings::default();
        let mut a = PowerAnalyzer::default();
        let t = feed_hz(&mut a, &cfg, 0.0, 0.5, 10.0, 12.0, 0.5);
        let mut dev = spm(12.0, 1.0, false, t);
        dev.trip_ocp = true;
        a.on_device(&dev, t);
        assert!(a.active(EventKind::Protection));
        assert_eq!(dev.protection_text().as_deref(), Some("OCP"));
        a.on_device(&dev, t + 1.0);
        assert_eq!(a.events.iter().filter(|e| e.kind == EventKind::Protection).count(), 1);
        dev.trip_ocp = false;
        a.on_device(&dev, t + 2.0);
        assert!(!a.active(EventKind::Protection));
        let ev = a.events.iter().find(|e| e.kind == EventKind::Protection).unwrap();
        assert_eq!(ev.t_end, Some(t + 2.0));
        // a fault without trip flags counts as well
        dev.mode = PsuMode::Fault;
        a.on_device(&dev, t + 3.0);
        assert!(a.active(EventKind::Protection));
        assert_eq!(dev.protection_text().as_deref(), Some("Fehler"));
    }

    #[test]
    fn spm_sample_skips_box_calibration() {
        let mut s =
            Store { calibration: Calibration { v_gain: 2.0, i_gain: 2.0, i_offset: 0.0 }, ..Default::default() };
        s.push_power_sample(PowerSample { t: 0.0, v: 1.0, i: 1.0 });
        s.push_power_sample_uncalibrated(PowerSample { t: 0.1, v: 1.0, i: 1.0 });
        let v: Vec<f64> = s.power.samples.iter().map(|p| p.v).collect();
        assert_eq!(v, vec![2.0, 1.0]);
    }

    #[test]
    fn hybrid_box_samples_see_cc_before_the_supply_reports_it() {
        // box at 100 Hz, SPM state at 10 Hz and 300 ms late with its CC
        let cfg = AnalysisSettings::default();
        let mut a = PowerAnalyzer::default();
        let mut t = 0.0;
        for _ in 0..20 {
            a.on_device(&spm(19.0, 1.0, true, t), t);
            t = feed_hz(&mut a, &cfg, t, 0.1, 100.0, 19.0, 0.5);
        }
        for k in 0..10 {
            let mut dev = spm(19.0, 1.0, true, t);
            if k >= 3 {
                dev.mode = PsuMode::Cc;
            }
            a.on_device(&dev, t);
            t = feed_hz(&mut a, &cfg, t, 0.1, 100.0, 15.0, 1.0);
        }
        assert!(a.events.iter().all(|e| e.kind != EventKind::Dropout), "{:?}", a.events);
        assert!(a.active(EventKind::CurrentLimit));
        assert_eq!(a.mode, RegMode::Cc);
    }

    #[test]
    fn lost_supply_leaves_its_set_points_as_learned() {
        let cfg = AnalysisSettings::default();
        let mut a = PowerAnalyzer::default();
        // the box alone learns 19 V
        let t = feed_hz(&mut a, &cfg, 0.0, 1.0, 100.0, 19.0, 0.0);
        assert!((a.learned_vset.unwrap() - 19.0).abs() < 1e-9);
        // then the SPM sets 5 V / 2 A and the board draws 0.3 A
        a.on_device(&spm(5.0, 2.0, true, t), t);
        let t = feed_hz(&mut a, &cfg, t, 1.0, 100.0, 5.0, 0.3);
        a.on_device_lost(t);
        assert_eq!(a.learned_vset, Some(5.0));
        assert_eq!(a.learned_iset, Some(2.0));
        feed_hz(&mut a, &cfg, t, 1.0, 100.0, 5.0, 0.3);
        assert!(a.events.is_empty(), "{:?}", a.events);
        assert_eq!(a.learned_iset, Some(2.0));
    }

    #[test]
    fn clearing_keeps_the_ramp_window() {
        let mut s = Store::default();
        let mut dev = spm(12.0, 1.0, false, 0.0);
        s.set_psu(dev.clone());
        dev.output_on = Some(true);
        dev.mode = PsuMode::Cv;
        dev.updated = Some(s.now());
        s.set_psu(dev);
        s.clear_all();
        let t0 = s.now();
        let mut v: f64 = 2.0;
        let mut t = t0;
        while v < 12.0 {
            v = (v + 0.055).min(12.0);
            s.push_power_sample_uncalibrated(PowerSample { t, v, i: v / 50.0 });
            t += 0.01;
        }
        assert!(s.power.analyzer.events.is_empty(), "{:?}", s.power.analyzer.events);
    }

    #[test]
    fn stale_output_off_does_not_force_off() {
        let cfg = AnalysisSettings::default();
        let mut a = PowerAnalyzer::default();
        // no MEAS:ALL:INFO?: no mode, output state read once at t = 0
        let dev = PsuState {
            v_set: Some(19.0),
            i_set: Some(1.0),
            output_on: Some(false),
            output_read: Some(0.0),
            ..Default::default()
        };
        a.on_device(&dev, 0.0);
        let t = feed_hz(&mut a, &cfg, 0.0, 0.5, 10.0, 0.0, 0.0);
        assert_eq!(a.mode, RegMode::Off);
        // switched on at the front panel, the board pulls the limit
        feed_hz(&mut a, &cfg, t, 3.0, 10.0, 15.0, 1.0);
        assert_eq!(a.mode, RegMode::Cc);
        assert!(a.active(EventKind::CurrentLimit));
    }

    #[test]
    fn parses_scpi_function() {
        assert_eq!(DmmFunction::from_scpi("\"VOLT AC\""), DmmFunction::VoltAc);
        assert_eq!(DmmFunction::from_scpi("VOLT"), DmmFunction::VoltDc);
        assert_eq!(DmmFunction::from_scpi("CURR:AC"), DmmFunction::CurrAc);
        assert_eq!(DmmFunction::from_scpi("RES"), DmmFunction::Res);
        assert_eq!(DmmFunction::from_scpi("NONe"), DmmFunction::Unknown);
    }
}
