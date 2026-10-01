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
    Marker,
}

impl EventKind {
    pub fn label(self) -> &'static str {
        match self {
            EventKind::Dropout => "Spannungseinbruch",
            EventKind::CurrentLimit => "Strombegrenzung (CC)",
            EventKind::Short => "Kurzschluss",
            EventKind::Marker => "Marker",
        }
    }
}

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
}

impl PowerAnalyzer {
    pub fn reset_stats(&mut self) {
        self.energy_wh = 0.0;
        self.charge_ah = 0.0;
        self.v = MinMax::default();
        self.i = MinMax::default();
        self.p = MinMax::default();
    }

    pub fn v_ref(&self, s: &AnalysisSettings) -> Option<f64> {
        s.v_set.or(self.learned_vset).or(self.v_ema)
    }

    pub fn i_limit(&self, s: &AnalysisSettings) -> Option<f64> {
        s.i_set.or(self.learned_iset)
    }

    pub fn v_set_effective(&self, s: &AnalysisSettings) -> Option<f64> {
        s.v_set.or(self.learned_vset)
    }

    pub fn active(&self, kind: EventKind) -> bool {
        let idx = match kind {
            EventKind::Dropout => self.open_dropout,
            EventKind::CurrentLimit => self.open_cc,
            EventKind::Short => self.open_short,
            EventKind::Marker => None,
        };
        idx.is_some()
    }

    pub fn push(&mut self, s: &PowerSample, cfg: &AnalysisSettings) {
        let p = s.p();
        if let Some(last) = self.last_t {
            let dt = (s.t - last).clamp(0.0, 1.0);
            self.energy_wh += p * dt / 3600.0;
            self.charge_ah += s.i * dt / 3600.0;
        }
        self.last_t = Some(s.t);
        self.v.add(s.v);
        self.i.add(s.i);
        self.p.add(p);

        self.learn(s, cfg);

        let v_ref = self.v_ref(cfg);
        let i_lim = self.i_limit(cfg);

        // --- regulation mode -------------------------------------------------
        let raw = if s.v < 0.05 && s.i.abs() < 0.002 {
            RegMode::Off
        } else if i_lim.is_some_and(|lim| lim > 0.0 && s.i >= lim * (1.0 - cfg.cc_tol_pct / 100.0)) {
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
        // 20 ms hysteresis keeps the badge from flickering on noise.
        if self.mode != self.pending_mode && s.t - self.pending_since >= 0.02 {
            self.mode = self.pending_mode;
        }

        // --- events ----------------------------------------------------------
        let is_short = s.v < cfg.short_volts && s.i > 0.01 && v_ref.is_some_and(|vr| vr > cfg.short_volts * 3.0);
        let is_cc = raw == RegMode::Cc && !is_short;
        let is_dropout = !is_short
            && !is_cc
            && v_ref.is_some_and(|vr| vr > 0.5 && s.v < vr * (1.0 - cfg.dropout_pct / 100.0))
            && raw != RegMode::Off;

        self.track(EventKind::Short, is_short, s);
        self.track(EventKind::CurrentLimit, is_cc, s);
        self.track(EventKind::Dropout, is_dropout, s);
        self.learn_limit_from_plateau(is_dropout, s, cfg);

        // Reference follows the voltage slowly, but not during a dip, so the
        // dip itself never becomes the new normal.
        if !is_dropout && !is_short && !is_cc && s.v > 0.05 {
            let alpha = 0.002; // ~5 s at 100 Hz
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
        if !is_dropout || !cfg.auto_learn_iset || cfg.i_set.is_some() {
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
        let flat = d.2 >= 10 && s.t - d.3 >= 0.15 && mean > 0.01 && std < mean * 0.005;
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
        // Unloaded output sits exactly at the set voltage.
        if cfg.auto_learn_vset && s.i.abs() < 0.003 && s.v > 0.3 {
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
        if cfg.auto_learn_iset && s.v < cfg.short_volts && s.i > 0.01 {
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
            for open in [&mut self.open_dropout, &mut self.open_cc, &mut self.open_short] {
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
    pub conn: ConnState,
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

    pub fn push_power_sample(&mut self, raw: PowerSample) {
        let s = self.calibration.apply(raw);
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

    pub fn add_marker(&mut self) {
        let t = self.now();
        self.power.analyzer.add_marker(t);
    }

    pub fn clear_all(&mut self) {
        self.power.samples.clear();
        self.power.analyzer = PowerAnalyzer::default();
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

    #[test]
    fn parses_scpi_function() {
        assert_eq!(DmmFunction::from_scpi("\"VOLT AC\""), DmmFunction::VoltAc);
        assert_eq!(DmmFunction::from_scpi("VOLT"), DmmFunction::VoltDc);
        assert_eq!(DmmFunction::from_scpi("CURR:AC"), DmmFunction::CurrAc);
        assert_eq!(DmmFunction::from_scpi("RES"), DmmFunction::Res);
        assert_eq!(DmmFunction::from_scpi("NONe"), DmmFunction::Unknown);
    }
}
