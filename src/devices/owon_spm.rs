//! OWON SPM3051 / SPM6053 / SPM3103 / SPM6103: programmable bench supply
//! with a built-in 4½-digit multimeter, SCPI over a USB serial port (CH340).
//! The SPE models (same supply, no multimeter) and the "multicomp pro
//! MP7111…" rebadges work too.
//!
//! One thread talks to the supply. Each cycle (10 Hz) it first sends what
//! the UI asked for, then reads `MEAS:ALL:INFO?` (voltage, current, power,
//! protection flags, CV/CC) and finally at most one side query: a read-back
//! after a set command, the set points (so turning the knob shows up), the
//! protection thresholds and output state, or else the multimeter.
//!
//! Safety rules: the driver never switches the output on by itself, never
//! changes it on connect, disconnect or exit (a board under repair must not
//! lose power because the app closed) and never sends `*RST`, which is a
//! factory reset on some units.

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use eframe::egui;

use super::{DmmCommand, PsuCommand, ScpiLink, ScpiPort, stopped};
use crate::model::{ConnState, DmmFunction, PowerSample, PsuMode, PsuState, Shared};

/// Wait per query attempt; the SPM answers in 10–20 ms.
const QUERY_TIMEOUT: Duration = Duration::from_millis(300);
/// `*IDN?` gets ~1 s (two attempts) before the other baud rate is tried.
const IDN_TIMEOUT: Duration = Duration::from_millis(500);
const RECONNECT_DELAY: Duration = if cfg!(test) { Duration::from_millis(200) } else { Duration::from_secs(2) };
/// One set point (alternating `VOLT?`/`CURR?`) is read back this often, so
/// the knob on the supply shows up.
const SETPOINT_EVERY: Duration = Duration::from_millis(250);
/// Protection thresholds and output state.
const SLOW_EVERY: Duration = Duration::from_secs(5);
/// Failed transactions in a row before reconnecting.
const MAX_FAILS: u32 = 3;
/// After `FUNC:…` the multimeter needs ~0.8 s plus autorange.
const FUNC_SWITCH_TIMEOUT: Duration = Duration::from_secs(2);
const DMM_PAUSE: Duration = Duration::from_secs(5);

pub struct SpmConfig {
    pub port: String,
    pub baud: u32,
    /// Also read the built-in multimeter (DmmKind::OwonSpm).
    pub with_dmm: bool,
    /// Push V/I samples. Off in the hybrid setup, where the PowerMon box
    /// delivers the samples and the SPM only set points, mode and control.
    pub push_samples: bool,
    /// Send `SYST:REM`, which locks the front panel.
    pub lock_panel: bool,
    pub poll_hz: f64,
}

impl Default for SpmConfig {
    fn default() -> Self {
        Self {
            port: String::new(),
            baud: 115_200,
            with_dmm: false,
            push_samples: true,
            lock_panel: false,
            poll_hz: 10.0,
        }
    }
}

// ------------------------------------------------------------------ parsers

#[derive(Clone, Debug, PartialEq)]
pub struct SpmIdn {
    pub raw: String,
    pub vendor: String,
    pub model: String,
    /// SPM yes, SPE no.
    pub has_dmm: bool,
}

impl SpmIdn {
    /// "OWON SPM6103"
    pub fn name(&self) -> String {
        format!("{} {}", self.vendor, self.model)
    }

    /// Voltage and current limits of the model, `None` if unknown.
    pub fn limits(&self) -> Option<(f64, f64)> {
        let m = self.model.to_ascii_uppercase();
        let digits = m.strip_prefix("SPM").or_else(|| m.strip_prefix("SPE"))?;
        [("3051", 30.0, 5.0), ("6053", 60.0, 5.0), ("3103", 30.0, 10.0), ("6103", 60.0, 10.0)]
            .into_iter()
            .find(|(k, ..)| digits.starts_with(k))
            .map(|(_, v, i)| (v, i))
    }
}

/// `OWON,SPM3051,24460467,FV:V2.0.0` or `multicomp pro,MP711134,…`.
pub fn parse_idn(s: &str) -> Result<SpmIdn, String> {
    let raw = s.trim().to_string();
    let mut parts = raw.split(',').map(str::trim);
    let vendor = parts.next().unwrap_or_default().to_string();
    let model = parts.next().unwrap_or_default().to_string();
    let (v, m) = (vendor.to_ascii_lowercase(), model.to_ascii_uppercase());
    let has_dmm = match (v.as_str(), m.get(..3)) {
        ("owon", Some("SPM")) => true,
        ("owon", Some("SPE")) => false,
        ("multicomp pro", _) if m.starts_with("MP7111") => true,
        _ => return Err(format!("Kein OWON SPM/SPE: {raw}")),
    };
    Ok(SpmIdn { raw, vendor, model, has_dmm })
}

/// Answer of `MEAS:ALL:INFO?`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AllInfo {
    pub v: f64,
    pub i: f64,
    pub p: f64,
    pub trip_ovp: bool,
    pub trip_ocp: bool,
    pub trip_otp: bool,
    pub mode: PsuMode,
}

fn fields(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| c == ',' || c.is_whitespace()).filter(|f| !f.is_empty())
}

/// `5.010,0.000,0.000,OFF,OFF,OFF,1` → V, I, P, OVP, OCP, OTP, mode
/// (0 = standby, 1 = CV, 2 = CC, 3 = fault). Exactly 7 fields, so the
/// answer to another query is never taken for this one.
pub fn parse_all_info(s: &str) -> Option<AllInfo> {
    let f: Vec<&str> = fields(s).collect();
    let [v, i, p, ovp, ocp, otp, mode] = f.as_slice() else { return None };
    Some(AllInfo {
        v: v.parse().ok()?,
        i: i.parse().ok()?,
        p: p.parse().ok()?,
        trip_ovp: parse_onoff(ovp)?,
        trip_ocp: parse_onoff(ocp)?,
        trip_otp: parse_onoff(otp)?,
        mode: match *mode {
            "0" => PsuMode::Standby,
            "1" => PsuMode::Cv,
            "2" => PsuMode::Cc,
            "3" => PsuMode::Fault,
            _ => PsuMode::Unknown,
        },
    })
}

/// `MEAS:ALL?`: `V,I` (real hardware) or `V I P` (manual).
pub fn parse_vi(s: &str) -> Option<(f64, f64)> {
    let f: Vec<&str> = fields(s).collect();
    let [v, i, rest @ ..] = f.as_slice() else { return None };
    if rest.len() > 1 {
        return None; // e.g. a stray `MEAS:ALL:INFO?` answer
    }
    Some((v.parse().ok()?, i.parse().ok()?))
}

/// A single number such as `5.400`; a unit or SI prefix is tolerated.
/// Several fields (the answer to another query) are rejected.
pub fn parse_number(s: &str) -> Option<f64> {
    let mut f = fields(s);
    let first = f.next()?;
    if f.next().is_some() {
        return None;
    }
    parse_si_value(first).filter(|v| v.is_finite())
}

/// `ON`/`OFF` or `1`/`0`.
pub fn parse_onoff(s: &str) -> Option<bool> {
    match s.trim().to_ascii_uppercase().as_str() {
        "ON" | "1" => Some(true),
        "OFF" | "0" => Some(false),
        _ => None,
    }
}

/// Length of the leading number (sign, digits, point, exponent).
fn numeric_prefix_len(t: &str) -> usize {
    let b = t.as_bytes();
    let mut k = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let digits_start = k;
    while k < b.len() && (b[k].is_ascii_digit() || b[k] == b'.') {
        k += 1;
    }
    if !b[digits_start..k].iter().any(u8::is_ascii_digit) {
        return 0;
    }
    if matches!(b.get(k), Some(b'e' | b'E')) {
        let mut j = k + 1;
        if matches!(b.get(j), Some(b'+' | b'-')) {
            j += 1;
        }
        let exp_start = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > exp_start {
            k = j;
        }
    }
    k
}

/// A value with optional SI prefix and unit: `+0.0006V`, `200k Ohm`,
/// `4.7 uF`, `12.5mA`, `1.2MOhm`. `OL` (overload) gives NaN.
pub fn parse_si_value(s: &str) -> Option<f64> {
    let t = s.trim().trim_matches('"');
    let up = t.to_ascii_uppercase();
    if up.trim_start_matches(['+', '-']).starts_with("OL") || up.contains("OVER") {
        return Some(f64::NAN);
    }
    let n = numeric_prefix_len(t);
    if n == 0 {
        return None;
    }
    let v: f64 = t[..n].parse().ok()?;
    let factor = match t[n..].trim_start().chars().next() {
        Some('p') => 1e-12,
        Some('n') => 1e-9,
        Some('u' | 'µ' | 'μ') => 1e-6,
        Some('m') => 1e-3,
        Some('k' | 'K') => 1e3,
        Some('M') => 1e6,
        Some('G') => 1e9,
        _ => 1.0,
    };
    Some(v * factor)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DmmReading {
    pub function: DmmFunction,
    /// NaN on overload.
    pub value: f64,
}

/// `CONF:ALL?` → `VOLT:DC,+0.0006V,AUTO,2V` or `RES,OL,AUTO,200k Ohm`.
pub fn parse_dmm_conf_all(s: &str) -> Option<DmmReading> {
    let mut parts = s.trim().trim_matches('"').split(',');
    let function = DmmFunction::from_scpi(parts.next()?);
    let value = parse_si_value(parts.next()?)?;
    (function != DmmFunction::Unknown).then_some(DmmReading { function, value })
}

/// Fallback `CONFigure?` → `<function> <value>`; also takes the
/// `CONF:ALL?` format.
pub fn parse_dmm_conf(s: &str) -> Option<DmmReading> {
    if s.contains(',') {
        return parse_dmm_conf_all(s);
    }
    let (f, v) = s.trim().trim_matches('"').split_once(char::is_whitespace)?;
    let function = DmmFunction::from_scpi(f);
    let value = parse_si_value(v)?;
    (function != DmmFunction::Unknown).then_some(DmmReading { function, value })
}

/// Rounds a set value to the supply's 3 decimals and keeps it within
/// [0, max]. NaN and infinity are rejected.
pub fn clamp_setpoint(x: f64, max: f64) -> Option<f64> {
    x.is_finite().then(|| (x.clamp(0.0, max.max(0.0)) * 1000.0).round() / 1000.0)
}

/// Highest OVP/OCP threshold the app sends: a little above the model's
/// output limit, so a threshold just over the maximum (e.g. 61 V on a 60 V
/// model) can be kept.
pub fn protection_limit(max: f64) -> f64 {
    max * 1.1
}

// ------------------------------------------------------------------ driver

fn is_timeout(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::TimedOut
}

pub fn open_serial(port: &str, baud: u32) -> Result<Box<dyn serialport::SerialPort>, String> {
    serialport::new(port, baud).timeout(Duration::from_millis(10)).open().map_err(|e| format!("{port}: {e}"))
}

/// Opens the port and identifies the supply. If `*IDN?` gets no (usable)
/// answer, the other of 115200/9600 baud is tried once.
pub fn connect<P: ScpiPort>(
    baud: u32,
    open: &mut impl FnMut(u32) -> Result<P, String>,
    stop: &AtomicBool,
) -> Result<(ScpiLink<P>, SpmIdn, u32), String> {
    let other = if baud == 9_600 { 115_200 } else { 9_600 };
    let mut last_err = format!("keine Antwort auf *IDN? ({baud} und {other} Baud)");
    let mut wrong_device = None;
    for b in [baud, other] {
        if stopped(stop) {
            return Err("abgebrochen".into());
        }
        let mut link = ScpiLink::new(open(b)?);
        link.timeout = IDN_TIMEOUT;
        link.drain();
        match link.query("*IDN?") {
            Ok(answer) => match parse_idn(&answer) {
                Ok(idn) => {
                    link.timeout = QUERY_TIMEOUT;
                    return Ok((link, idn, b));
                }
                // Could be line noise at the wrong baud rate: try the other.
                Err(e) => wrong_device = Some(e),
            },
            Err(e) if is_timeout(&e) => {}
            Err(e) => last_err = e.to_string(),
        }
    }
    Err(wrong_device.unwrap_or(last_err))
}

/// Runs the SPM on a real serial port until `stop`.
pub fn run(
    cfg: SpmConfig,
    store: Shared,
    ctx: egui::Context,
    psu_rx: Receiver<PsuCommand>,
    dmm_rx: Receiver<DmmCommand>,
    stop: Arc<AtomicBool>,
) {
    let port = cfg.port.clone();
    run_with(cfg, store, ctx, psu_rx, dmm_rx, stop, move |baud| open_serial(&port, baud));
}

/// The driver with any port: `open(baud)` returns a fresh connection. The
/// simulator runs exactly this code against a fake SPM.
pub fn run_with<P: ScpiPort>(
    cfg: SpmConfig,
    store: Shared,
    ctx: egui::Context,
    psu_rx: Receiver<PsuCommand>,
    dmm_rx: Receiver<DmmCommand>,
    stop: Arc<AtomicBool>,
    mut open: impl FnMut(u32) -> Result<P, String>,
) {
    let set_conn = |c: ConnState| {
        let mut s = store.lock().unwrap();
        s.power.psu_conn = c.clone();
        if cfg.push_samples {
            s.power.conn = c.clone();
        }
        if cfg.with_dmm {
            s.dmm.conn = c;
        }
        drop(s);
        ctx.request_repaint();
    };
    set_conn(ConnState::Connecting);

    // An "output off" that could not be delivered yet (see `session`).
    let mut pending_off = false;
    // Reconnect loop: unplugging the supply mid-video shouldn't need a click.
    while !stopped(&stop) {
        let result = session(&cfg, &mut open, &store, &ctx, &psu_rx, &dmm_rx, &stop, &mut pending_off);
        store.lock().unwrap().clear_psu();
        match result {
            Ok(()) => break,
            Err(e) => {
                set_conn(ConnState::Error(format!("{e} – neuer Versuch …")));
                let until = Instant::now() + RECONNECT_DELAY;
                while Instant::now() < until && !stopped(&stop) {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }
    set_conn(ConnState::Disconnected);
}

#[allow(clippy::too_many_arguments)]
fn session<P: ScpiPort>(
    cfg: &SpmConfig,
    open: &mut impl FnMut(u32) -> Result<P, String>,
    store: &Shared,
    ctx: &egui::Context,
    psu_rx: &Receiver<PsuCommand>,
    dmm_rx: &Receiver<DmmCommand>,
    stop: &AtomicBool,
    pending_off: &mut bool,
) -> Result<(), String> {
    let (link, idn, baud) = connect(cfg.baud, open, stop)?;
    // Clicks from while there was no connection are stale: never replay
    // them (and never send anything on connect) – except "output off". That
    // one is always safe late, and the user pressed it (panic key O) while
    // the supply was unreachable; dropping it would leave the output on.
    while let Ok(cmd) = psu_rx.try_recv() {
        *pending_off |= cmd == PsuCommand::Output(false);
    }
    while dmm_rx.try_recv().is_ok() {}

    let mut s = Session::new(cfg, store, ctx, link, &idn);
    let mut result = s.init(&idn, baud);
    if result.is_ok() && *pending_off {
        result = s.apply_psu(PsuCommand::Output(false)).map_err(|e| e.to_string());
        *pending_off = result.is_err();
    }
    let result = result.and_then(|()| s.run(psu_rx, dmm_rx, stop));
    // Front panel back to the user. The output stays as it is.
    let _ = s.link.send("SYST:LOC");
    result
}

/// How voltage and current are read; probed once per connection.
#[derive(Clone, Copy, Debug, PartialEq)]
enum MeasQuery {
    /// `MEAS:ALL:INFO?`: V, I, P, protection flags and mode in one go.
    Info,
    /// `MEAS:ALL?`: V and I.
    All,
    /// `MEAS:VOLT?` + `MEAS:CURR?`.
    Separate,
}

/// Side queries, one per cycle at most.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Side {
    VSet,
    ISet,
    Ovp,
    Ocp,
    Output,
}

impl Side {
    fn query(self) -> &'static str {
        match self {
            Side::VSet => "VOLT?",
            Side::ISet => "CURR?",
            Side::Ovp => "VOLT:LIM?",
            Side::Ocp => "CURR:LIM?",
            Side::Output => "OUTP?",
        }
    }
}

struct DmmPoll {
    /// `CONF:ALL?` or the `CONFigure?` fallback.
    query: &'static str,
    name: String,
    fails: u32,
    paused_until: Option<Instant>,
    /// Function just selected; readings of the old one are ignored until
    /// the meter reports the new one.
    pending: Option<(DmmFunction, Instant)>,
    connected: bool,
}

struct Session<'a, P: ScpiPort> {
    cfg: &'a SpmConfig,
    store: &'a Shared,
    ctx: &'a egui::Context,
    link: ScpiLink<P>,
    st: PsuState,
    meas: MeasQuery,
    fails: u32,
    /// Read-backs after a set command.
    forced: VecDeque<Side>,
    slow: VecDeque<Side>,
    /// When the next set point read is due, and which one it is.
    next_vi: Instant,
    vi_turn: usize,
    last_slow: Instant,
    dmm: Option<DmmPoll>,
}

impl<'a, P: ScpiPort> Session<'a, P> {
    fn new(cfg: &'a SpmConfig, store: &'a Shared, ctx: &'a egui::Context, link: ScpiLink<P>, idn: &SpmIdn) -> Self {
        // Unknown model: 60 V / 10 A is the largest SPM.
        let (v_max, i_max) = idn.limits().unwrap_or((60.0, 10.0));
        Self {
            cfg,
            store,
            ctx,
            link,
            st: PsuState { model: idn.name(), v_max, i_max, ..Default::default() },
            meas: MeasQuery::Info,
            fails: 0,
            forced: VecDeque::new(),
            slow: VecDeque::new(),
            next_vi: Instant::now(),
            vi_turn: 0,
            last_slow: Instant::now(),
            dmm: None,
        }
    }

    /// Probes what the supply understands and reads its settings once.
    /// Sends no set command (except `SYST:REM` if the user wants the panel
    /// locked).
    fn init(&mut self, idn: &SpmIdn, baud: u32) -> Result<(), String> {
        self.meas = self.probe_measure()?;
        for side in [Side::VSet, Side::ISet, Side::Ovp, Side::Ocp, Side::Output] {
            self.read_side(side)?;
        }
        // An unknown model (e.g. a rebadge) keeps 60 V / 10 A as ceiling.
        // OVP/OCP are user settings, not model limits: deriving the ceiling
        // from them would lock the user below their own threshold. Values
        // the supply can't do it answers with ERR.
        let dmm_error = if !self.cfg.with_dmm {
            None
        } else if !idn.has_dmm {
            Some(format!("{} hat kein Multimeter", idn.model))
        } else {
            match self.probe_dmm()? {
                Some(query) => {
                    self.dmm = Some(DmmPoll {
                        query,
                        name: format!("{} (eingebaut)", idn.name()),
                        fails: 0,
                        paused_until: None,
                        pending: None,
                        connected: false,
                    });
                    None
                }
                None => Some("Multimeter im Netzteil antwortet nicht (CONF:ALL?)".into()),
            }
        };
        if self.cfg.lock_panel {
            self.link.send_checked("SYST:REM").map_err(|e| e.to_string())?;
        }

        let text =
            if baud == self.cfg.baud { self.st.model.clone() } else { format!("{} · {baud} Baud", self.st.model) };
        let mut s = self.store.lock().unwrap();
        s.power.psu_conn = ConnState::Connected(text.clone());
        if self.cfg.push_samples {
            s.power.conn = ConnState::Connected(text);
        }
        if let Some(e) = dmm_error {
            // The supply keeps running without its multimeter.
            s.dmm.conn = ConnState::Error(e);
        }
        s.set_psu(self.st.clone());
        drop(s);
        self.ctx.request_repaint();
        Ok(())
    }

    fn probe_measure(&mut self) -> Result<MeasQuery, String> {
        if let Some(a) = self.query_soft("MEAS:ALL:INFO?")?
            && parse_all_info(&a).is_some()
        {
            return Ok(MeasQuery::Info);
        }
        if let Some(a) = self.query_soft("MEAS:ALL?")?
            && parse_vi(&a).is_some()
        {
            return Ok(MeasQuery::All);
        }
        let v = self.query_soft("MEAS:VOLT?")?.and_then(|a| parse_number(&a));
        let i = self.query_soft("MEAS:CURR?")?.and_then(|a| parse_number(&a));
        if v.is_some() && i.is_some() {
            return Ok(MeasQuery::Separate);
        }
        Err("Netzteil liefert keine Messwerte (MEAS:ALL:INFO?, MEAS:ALL?, MEAS:VOLT?)".into())
    }

    fn probe_dmm(&mut self) -> Result<Option<&'static str>, String> {
        for q in ["CONF:ALL?", "CONF?"] {
            if self.query_soft(q)?.and_then(|a| parse_dmm_conf(&a)).is_some() {
                return Ok(Some(q));
            }
        }
        Ok(None)
    }

    /// A query whose timeout is not an error (`None`); only a dead port is.
    fn query_soft(&mut self, cmd: &str) -> Result<Option<String>, String> {
        match self.link.query(cmd) {
            Ok(a) => Ok(Some(a)),
            Err(e) if is_timeout(&e) => {
                self.link.drain();
                Ok(None)
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// Counts a failed transaction; too many in a row → reconnect.
    fn failed(&mut self, what: &str) -> Result<(), String> {
        self.fails += 1;
        self.link.drain();
        if self.fails >= MAX_FAILS { Err(format!("Netzteil antwortet nicht ({what})")) } else { Ok(()) }
    }

    fn publish(&self) {
        self.store.lock().unwrap().set_psu(self.st.clone());
        self.ctx.request_repaint();
    }

    fn run(
        &mut self,
        psu_rx: &Receiver<PsuCommand>,
        dmm_rx: &Receiver<DmmCommand>,
        stop: &AtomicBool,
    ) -> Result<(), String> {
        let period = Duration::from_secs_f64(1.0 / self.cfg.poll_hz.clamp(0.5, 50.0));
        let mut next = Instant::now();
        while !stopped(stop) {
            self.handle_commands(psu_rx, dmm_rx)?;
            self.poll_supply()?;
            self.side_query()?;
            next += period;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                next = now;
            }
        }
        Ok(())
    }

    fn handle_commands(&mut self, psu_rx: &Receiver<PsuCommand>, dmm_rx: &Receiver<DmmCommand>) -> Result<(), String> {
        while let Ok(cmd) = psu_rx.try_recv() {
            self.apply_psu(cmd).map_err(|e| e.to_string())?;
        }
        while let Ok(cmd) = dmm_rx.try_recv() {
            // The SPM's meter has no rate setting; DmmCommand::SetRate is ignored.
            if let DmmCommand::SetFunction(f) = cmd
                && self.dmm.is_some()
                && let Some(scpi) = f.scpi_func_spm()
            {
                if self.link.send_checked(scpi).map_err(|e| e.to_string())? {
                    if let Some(d) = self.dmm.as_mut() {
                        d.pending = Some((f, Instant::now()));
                    }
                    self.store.lock().unwrap().set_dmm_function(f);
                } else {
                    self.st.note = Some(format!("Netzteil hat „{scpi}“ abgelehnt"));
                    self.publish();
                }
            }
        }
        Ok(())
    }

    fn apply_psu(&mut self, cmd: PsuCommand) -> io::Result<()> {
        let fmt = |prefix: &str, v: Option<f64>| v.map(|v| format!("{prefix} {v:.3}"));
        let (scpi, side) = match cmd {
            PsuCommand::SetVoltage(v) => (fmt("VOLT", clamp_setpoint(v, self.st.v_max)), Side::VSet),
            PsuCommand::SetCurrent(i) => (fmt("CURR", clamp_setpoint(i, self.st.i_max)), Side::ISet),
            PsuCommand::SetOvp(v) => (fmt("VOLT:LIM", clamp_setpoint(v, protection_limit(self.st.v_max))), Side::Ovp),
            PsuCommand::SetOcp(i) => (fmt("CURR:LIM", clamp_setpoint(i, protection_limit(self.st.i_max))), Side::Ocp),
            // Only ever sent because the user clicked the output button.
            PsuCommand::Output(on) => (Some(if on { "OUTP ON" } else { "OUTP OFF" }.to_string()), Side::Output),
        };
        match scpi {
            None => self.st.note = Some("Ungültiger Wert, nicht gesendet".into()),
            Some(scpi) => {
                let accepted = self.link.send_checked(&scpi)?;
                self.st.note = (!accepted).then(|| format!("Netzteil hat „{scpi}“ abgelehnt"));
                if !self.forced.contains(&side) {
                    self.forced.push_back(side);
                }
            }
        }
        self.publish();
        Ok(())
    }

    fn measure(&mut self) -> io::Result<Option<(f64, f64, Option<AllInfo>)>> {
        Ok(match self.meas {
            MeasQuery::Info => parse_all_info(&self.link.query("MEAS:ALL:INFO?")?).map(|a| (a.v, a.i, Some(a))),
            MeasQuery::All => parse_vi(&self.link.query("MEAS:ALL?")?).map(|(v, i)| (v, i, None)),
            MeasQuery::Separate => {
                let v = parse_number(&self.link.query("MEAS:VOLT?")?);
                let i = parse_number(&self.link.query("MEAS:CURR?")?);
                v.zip(i).map(|(v, i)| (v, i, None))
            }
        })
    }

    fn poll_supply(&mut self) -> Result<(), String> {
        let t0 = self.store.lock().unwrap().now();
        let reading = match self.measure() {
            Ok(Some(r)) => r,
            Ok(None) => return self.failed("unlesbare Messwerte"),
            Err(e) if is_timeout(&e) => return self.failed("Messwerte"),
            Err(e) => return Err(e.to_string()),
        };
        self.fails = 0;
        let (v, i, info) = reading;
        let mut s = self.store.lock().unwrap();
        let t1 = s.now();
        if let Some(info) = info {
            let was_tripped = self.st.protection();
            self.st.mode = info.mode;
            self.st.trip_ovp = info.trip_ovp;
            self.st.trip_ocp = info.trip_ocp;
            self.st.trip_otp = info.trip_otp;
            self.st.updated = Some(t1);
            let on = match info.mode {
                PsuMode::Standby => Some(false),
                PsuMode::Cv | PsuMode::Cc => Some(true),
                PsuMode::Fault | PsuMode::Unknown => None,
            };
            if on.is_some() {
                self.st.output_on = on;
                self.st.output_read = Some(t1);
            }
            if self.st.protection() && !was_tripped && !self.forced.contains(&Side::Output) {
                self.forced.push_back(Side::Output);
            }
        }
        s.set_psu(self.st.clone());
        if self.cfg.push_samples {
            // Time stamp in the middle of the query; the SPM calibrates
            // itself, the box calibration doesn't apply.
            s.push_power_sample_uncalibrated(PowerSample { t: (t0 + t1) / 2.0, v, i });
        }
        drop(s);
        self.ctx.request_repaint();
        Ok(())
    }

    fn side_query(&mut self) -> Result<(), String> {
        let now = Instant::now();
        let side = if let Some(side) = self.forced.pop_front() {
            Some(side)
        } else if now >= self.next_vi {
            // A deadline, not "250 ms since the last one": the 10 Hz loop
            // then still averages one read per 250 ms.
            self.next_vi = (self.next_vi + SETPOINT_EVERY).max(now);
            // Without `MEAS:ALL:INFO?` nothing else tells whether the output
            // is on, so `OUTP?` joins the fast rotation.
            let turn: &[Side] = if self.meas == MeasQuery::Info {
                &[Side::VSet, Side::ISet]
            } else {
                &[Side::VSet, Side::ISet, Side::Output]
            };
            self.vi_turn = self.vi_turn.wrapping_add(1);
            Some(turn[self.vi_turn % turn.len()])
        } else {
            if now - self.last_slow >= SLOW_EVERY {
                self.last_slow = now;
                self.slow.extend([Side::Ovp, Side::Ocp, Side::Output]);
            }
            self.slow.pop_front()
        };
        match side {
            Some(side) => self.read_side(side),
            None => self.poll_dmm(),
        }
    }

    fn read_side(&mut self, side: Side) -> Result<(), String> {
        let Some(answer) = self.query_soft(side.query())? else {
            return self.failed(side.query());
        };
        // `ERR` means the supply doesn't know the query: keep what we have.
        if answer.eq_ignore_ascii_case("ERR") {
            self.fails = 0;
            return Ok(());
        }
        let parsed = match side {
            Side::VSet => parse_number(&answer).map(|v| self.st.v_set = Some(v)),
            Side::ISet => parse_number(&answer).map(|i| self.st.i_set = Some(i)),
            Side::Ovp => parse_number(&answer).map(|v| self.st.ovp = Some(v)),
            Side::Ocp => parse_number(&answer).map(|i| self.st.ocp = Some(i)),
            Side::Output => parse_onoff(&answer).map(|on| {
                self.st.output_on = Some(on);
                self.st.output_read = Some(self.store.lock().unwrap().now());
            }),
        };
        if parsed.is_none() {
            // Not the shape this query answers (e.g. a late answer to an
            // earlier one): out of step, so drain instead of guessing.
            return self.failed(side.query());
        }
        self.fails = 0;
        self.publish();
        Ok(())
    }

    fn poll_dmm(&mut self) -> Result<(), String> {
        let Some(d) = self.dmm.as_ref() else { return Ok(()) };
        if d.paused_until.is_some_and(|u| Instant::now() < u) {
            return Ok(());
        }
        let query = d.query;
        let reading = match self.link.query(query) {
            Ok(a) => {
                let r = parse_dmm_conf(&a);
                if r.is_none() && !a.eq_ignore_ascii_case("ERR") {
                    self.link.drain(); // out of step
                }
                r
            }
            Err(e) if is_timeout(&e) => {
                self.link.drain();
                None
            }
            Err(e) => return Err(e.to_string()),
        };
        let Some(d) = self.dmm.as_mut() else { return Ok(()) };
        let Some(r) = reading else {
            // Only the multimeter pauses; the supply keeps running.
            d.fails += 1;
            if d.fails >= MAX_FAILS {
                d.fails = 0;
                d.connected = false;
                d.paused_until = Some(Instant::now() + DMM_PAUSE);
                self.store.lock().unwrap().dmm.conn =
                    ConnState::Error("Multimeter im Netzteil antwortet nicht – neuer Versuch …".into());
            }
            return Ok(());
        };
        d.fails = 0;
        d.paused_until = None;
        if let Some((f, since)) = d.pending {
            if r.function != f && since.elapsed() < FUNC_SWITCH_TIMEOUT {
                return Ok(()); // still the old function
            }
            d.pending = None;
        }
        let mut s = self.store.lock().unwrap();
        if !d.connected {
            d.connected = true;
            s.dmm.conn = ConnState::Connected(d.name.clone());
        }
        s.set_dmm_function(r.function);
        s.push_dmm(r.value);
        drop(s);
        self.ctx.request_repaint();
        Ok(())
    }
}

// ------------------------------------------------------------------- probe

/// `powermeter --probe-spm <port> [baud]`: reads everything once and
/// measures how often the supply really refreshes its readings. Read-only:
/// sends queries only, never a set command, `SYST:REM` or `*RST`.
pub fn probe<P: ScpiPort>(
    baud: u32,
    mut open: impl FnMut(u32) -> Result<P, String>,
    out: &mut dyn io::Write,
) -> Result<(), String> {
    let stop = AtomicBool::new(false);
    let (mut link, idn, used) = connect(baud, &mut open, &stop)?;
    let w = |out: &mut dyn io::Write, line: String| {
        let _ = writeln!(out, "{line}");
    };
    w(out, format!("*IDN?            → {}   ({used} Baud)", idn.raw));
    let limits = idn.limits().map_or("unbekannt".into(), |(v, i)| format!("{v} V / {i} A"));
    w(
        out,
        format!("Modell           {} · {limits} · Multimeter: {}", idn.name(), if idn.has_dmm { "ja" } else { "nein" }),
    );

    w(out, String::new());
    w(out, "50 × MEAS:ALL:INFO?   (Zeit, Antwortzeit, Antwort; * = neuer Messwert)".into());
    let start = Instant::now();
    let (mut answered, mut last, mut changes) = (0, None::<String>, Vec::new());
    for _ in 0..50 {
        let t = start.elapsed().as_secs_f64();
        match link.query("MEAS:ALL:INFO?") {
            Ok(a) => {
                let rt = start.elapsed().as_secs_f64() - t;
                answered += 1;
                // Only V, I, P count: flags and mode don't change readings.
                let values: String = fields(&a).take(3).collect::<Vec<_>>().join(",");
                let new = last.as_ref() != Some(&values);
                if new {
                    changes.push(t);
                }
                w(out, format!("{:>8.1} ms  {:>4.0} ms  {a}{}", t * 1000.0, rt * 1000.0, if new { "  *" } else { "" }));
                last = Some(values);
            }
            Err(e) => {
                w(out, format!("{:>8.1} ms  {e}", t * 1000.0));
                link.drain();
            }
        }
    }
    let secs = start.elapsed().as_secs_f64();
    w(out, format!("→ {answered}/50 Antworten in {secs:.2} s ({:.1} Abfragen/s)", answered as f64 / secs));
    let per_s: Vec<String> =
        (0..secs.ceil() as usize).map(|k| changes.iter().filter(|t| (**t as usize) == k).count().to_string()).collect();
    w(
        out,
        format!(
            "→ {} verschiedene Messwerte = {:.1} pro Sekunde (je Sekunde: {})",
            changes.len(),
            changes.len() as f64 / secs,
            per_s.join(", ")
        ),
    );
    w(out, "  (Mit Last, die sich bewegt, messen – bei konstanten Werten zählt jeder nur einmal.)".into());

    w(out, String::new());
    for q in ["MEAS:ALL?", "VOLT?", "CURR?", "VOLT:LIM?", "CURR:LIM?", "OUTP?", "CONF:ALL?", "FUNC?"] {
        let t = Instant::now();
        match link.query(q) {
            Ok(a) => w(out, format!("{q:<16} → {a}   ({} ms)", t.elapsed().as_millis())),
            Err(e) => {
                w(out, format!("{q:<16} → {e}"));
                link.drain();
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::DeviceHandle;
    use crate::devices::sim::{FakeSpm, SharedSpm, SpmDevice};
    use crate::model::Store;
    use std::sync::mpsc::{self, Sender};

    #[test]
    fn parses_idn() {
        let idn = parse_idn("OWON,SPM3051,24460467,FV:V2.0.0\r").unwrap();
        assert_eq!(idn.name(), "OWON SPM3051");
        assert!(idn.has_dmm);
        assert_eq!(idn.limits(), Some((30.0, 5.0)));
        let idn = parse_idn("multicomp pro,MP711134,25280364,FV:V2.0.0").unwrap();
        assert!(idn.has_dmm);
        assert_eq!(idn.limits(), None);
        assert_eq!(parse_idn("OWON,SPM6103,1,FV").unwrap().limits(), Some((60.0, 10.0)));
        assert_eq!(parse_idn("owon,spm3103,1,FV").unwrap().limits(), Some((30.0, 10.0)));
        let spe = parse_idn("OWON,SPE6053,1,FV:V1.0").unwrap();
        assert!(!spe.has_dmm);
        assert_eq!(spe.limits(), Some((60.0, 5.0)));
        assert_eq!(parse_idn("OWON,XDM1241,1,V3").unwrap_err(), "Kein OWON SPM/SPE: OWON,XDM1241,1,V3");
        assert!(parse_idn("ERR").is_err());
        assert!(parse_idn("").is_err());
    }

    #[test]
    fn parses_all_info() {
        let a = parse_all_info("5.010,0.000,0.000,OFF,OFF,OFF,1\r\n").unwrap();
        assert_eq!((a.v, a.i, a.p), (5.01, 0.0, 0.0));
        assert!(!a.trip_ovp && !a.trip_ocp && !a.trip_otp);
        assert_eq!(a.mode, PsuMode::Cv);
        let a = parse_all_info("0.000 0.000 0.000 ON 0 1 3").unwrap();
        assert!(a.trip_ovp && !a.trip_ocp && a.trip_otp);
        assert_eq!(a.mode, PsuMode::Fault);
        assert_eq!(parse_all_info("12.000,1.000,12.000,OFF,OFF,OFF,2").unwrap().mode, PsuMode::Cc);
        assert_eq!(parse_all_info("0,0,0,OFF,OFF,OFF,0").unwrap().mode, PsuMode::Standby);
        assert_eq!(parse_all_info("ERR"), None);
        assert_eq!(parse_all_info("5.010,0.000"), None);
        assert_eq!(parse_all_info("5.010,0.000,0.000,OFF,OFF,OFF,1,9"), None);
    }

    #[test]
    fn parses_vi_numbers_and_flags() {
        assert_eq!(parse_vi("0.000,0.000"), Some((0.0, 0.0)));
        assert_eq!(parse_vi("1.000 2.000 2.000"), Some((1.0, 2.0)));
        assert_eq!(parse_vi("ERR"), None);
        assert_eq!(parse_vi("5.010,0.000,0.000,OFF,OFF,OFF,1"), None);
        assert_eq!(parse_number("5.400\r\n"), Some(5.4));
        // the answer to another query is not a set point
        assert_eq!(parse_number("18.230,1.000,18.230,OFF,OFF,OFF,2"), None);
        assert_eq!(parse_number("0.000,0.000"), None);
        assert_eq!(parse_number("4.900"), Some(4.9));
        assert_eq!(parse_number("12.000V"), Some(12.0));
        assert_eq!(parse_number("ERR"), None);
        assert_eq!(parse_number(""), None);
        assert_eq!(parse_onoff("ON"), Some(true));
        assert_eq!(parse_onoff("OFF\r"), Some(false));
        assert_eq!(parse_onoff("1"), Some(true));
        assert_eq!(parse_onoff("0"), Some(false));
        assert_eq!(parse_onoff("ERR"), None);
    }

    #[test]
    fn parses_si_values() {
        let close = |a: Option<f64>, b: f64| a.is_some_and(|a| (a - b).abs() <= b.abs() * 1e-12);
        assert!(close(parse_si_value("+0.0006V"), 0.0006));
        assert!(close(parse_si_value("12.5mV"), 0.0125));
        assert!(close(parse_si_value("4.7 uF"), 4.7e-6));
        assert!(close(parse_si_value("4.7µF"), 4.7e-6));
        assert!(close(parse_si_value("4.7001kOhm"), 4700.1));
        assert!(close(parse_si_value("200k Ohm"), 200e3));
        assert!(close(parse_si_value("1.5MOhm"), 1.5e6));
        assert!(close(parse_si_value("-1.2E-03A"), -0.0012));
        assert!(close(parse_si_value("470 Ohm"), 470.0));
        assert!(close(parse_si_value("10Ω"), 10.0));
        assert!(parse_si_value("OL").unwrap().is_nan());
        assert!(parse_si_value("-OL").unwrap().is_nan());
        assert_eq!(parse_si_value("AUTO"), None);
        assert_eq!(parse_si_value(""), None);
    }

    #[test]
    fn parses_multimeter_answers() {
        let r = parse_dmm_conf_all("VOLT:DC,+0.0006V,AUTO,2V").unwrap();
        assert_eq!(r.function, DmmFunction::VoltDc);
        assert!((r.value - 0.0006).abs() < 1e-12);
        let r = parse_dmm_conf_all("RES,OL,AUTO,200k Ohm").unwrap();
        assert_eq!(r.function, DmmFunction::Res);
        assert!(r.value.is_nan());
        let r = parse_dmm_conf_all("CURR:AC,+12.345mA,MANUAL,200mA").unwrap();
        assert_eq!(r.function, DmmFunction::CurrAc);
        assert!((r.value - 0.012345).abs() < 1e-12);
        assert_eq!(parse_dmm_conf_all("ERR"), None);
        let r = parse_dmm_conf("VOLT:DC 0.0006").unwrap();
        assert_eq!(r.function, DmmFunction::VoltDc);
        assert_eq!(parse_dmm_conf("CAP,+1.000uF,AUTO,2uF").unwrap().function, DmmFunction::Cap);
    }

    #[test]
    fn clamps_set_values() {
        assert_eq!(clamp_setpoint(12.0, 60.0), Some(12.0));
        assert_eq!(clamp_setpoint(99.0, 60.0), Some(60.0));
        assert_eq!(clamp_setpoint(-1.0, 60.0), Some(0.0));
        assert_eq!(clamp_setpoint(3.30049, 60.0), Some(3.3));
        assert_eq!(clamp_setpoint(f64::NAN, 60.0), None);
        assert_eq!(clamp_setpoint(f64::INFINITY, 60.0), None);
        assert_eq!(clamp_setpoint(70.0, protection_limit(60.0)), Some(66.0));
    }

    struct Rig {
        store: Shared,
        dev: SharedSpm,
        psu: Sender<PsuCommand>,
        dmm: Sender<DmmCommand>,
        handle: DeviceHandle,
    }

    fn start(cfg: SpmConfig) -> Rig {
        start_on(cfg, SpmDevice::shared())
    }

    fn start_on(cfg: SpmConfig, dev: SharedSpm) -> Rig {
        let store = Store::shared();
        let (psu, psu_rx) = mpsc::channel();
        let (dmm, dmm_rx) = mpsc::channel();
        let (s, d) = (store.clone(), dev.clone());
        let handle = DeviceHandle::spawn("spm-test", move |stop| {
            run_with(cfg, s, egui::Context::default(), psu_rx, dmm_rx, stop, move |_| Ok(FakeSpm::new(d.clone())))
        });
        Rig { store, dev, psu, dmm, handle }
    }

    fn wait_for(store: &Shared, secs: f64, what: &str, cond: impl Fn(&Store) -> bool) {
        let until = Instant::now() + Duration::from_secs_f64(secs);
        while Instant::now() < until {
            if cond(&store.lock().unwrap()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timeout: {what}");
    }

    #[test]
    fn end_to_end_samples_and_set_point_read_back() {
        let rig = start(SpmConfig::default());
        wait_for(&rig.store, 3.0, "samples", |s| s.power.psu_conn.is_connected() && s.power.samples.len() >= 5);
        {
            let s = rig.store.lock().unwrap();
            let psu = s.power.psu.as_ref().unwrap();
            assert_eq!(psu.model, "OWON SPM6103");
            assert_eq!((psu.v_max, psu.i_max), (60.0, 10.0));
            assert_eq!(psu.output_on, Some(true));
            assert!(psu.updated.is_some());
            assert!(s.power.conn.is_connected());
        }
        rig.psu.send(PsuCommand::SetVoltage(12.0)).unwrap();
        wait_for(&rig.store, 2.0, "read-back", |s| s.power.psu.as_ref().and_then(|p| p.v_set) == Some(12.0));
        // the supply's set point wins over one typed into the app
        let typed = crate::model::AnalysisSettings { v_set: Some(5.0), ..Default::default() };
        assert_eq!(rig.store.lock().unwrap().power.analyzer.v_set_effective(&typed), Some(12.0));
        // NaN never reaches the supply
        rig.psu.send(PsuCommand::SetCurrent(f64::NAN)).unwrap();
        wait_for(&rig.store, 2.0, "note", |s| s.power.psu.as_ref().is_some_and(|p| p.note.is_some()));

        let Rig { store, dev, handle, .. } = rig;
        drop(handle);
        let s = store.lock().unwrap();
        assert!(s.power.psu.is_none());
        assert_eq!(s.power.psu_conn, ConnState::Disconnected);
        let d = dev.lock().unwrap();
        assert!(d.output_on, "closing the app must not switch the output off");
        assert!(d.log.iter().any(|c| c == "VOLT 12.000"));
        assert_eq!(d.log.back().map(String::as_str), Some("SYST:LOC"));
        assert!(!d.log.iter().any(|c| c.starts_with("OUTP ") || c.contains("*RST") || c.starts_with("CURR ")));
    }

    #[test]
    fn end_to_end_recovers_from_dropped_answers() {
        let rig = start(SpmConfig::default());
        wait_for(&rig.store, 3.0, "connect", |s| s.power.psu_conn.is_connected());
        let n = rig.store.lock().unwrap().power.samples.len();
        // one lost answer: the retry covers it, no reconnect
        rig.dev.lock().unwrap().drop_replies = 1;
        wait_for(&rig.store, 2.0, "more samples", |s| s.power.samples.len() >= n + 5);
        assert!(rig.store.lock().unwrap().power.psu_conn.is_connected());
        // the supply goes quiet for a while: reconnect, then carry on
        rig.dev.lock().unwrap().drop_replies = 2 * MAX_FAILS;
        wait_for(&rig.store, 4.0, "error", |s| matches!(s.power.psu_conn, ConnState::Error(_)));
        wait_for(&rig.store, 4.0, "reconnect", |s| s.power.psu_conn.is_connected() && s.power.psu.is_some());
        let n = rig.store.lock().unwrap().power.samples.len();
        wait_for(&rig.store, 2.0, "samples again", |s| s.power.samples.len() >= n + 3);
    }

    #[test]
    fn end_to_end_multimeter_and_hybrid() {
        let cfg = SpmConfig { with_dmm: true, push_samples: false, ..Default::default() };
        let rig = start(cfg);
        wait_for(&rig.store, 3.0, "dmm", |s| s.dmm.conn.is_connected() && s.dmm.samples.len() >= 2);
        assert_eq!(rig.store.lock().unwrap().dmm.function, DmmFunction::VoltDc);
        rig.dmm.send(DmmCommand::SetFunction(DmmFunction::Res)).unwrap();
        wait_for(&rig.store, 3.0, "Ω readings", |s| {
            s.dmm.function == DmmFunction::Res && s.dmm.samples.back().is_some_and(|d| d.value > 1000.0)
        });
        let s = rig.store.lock().unwrap();
        // hybrid: the SPM controls, the samples come from elsewhere
        assert!(s.power.samples.is_empty());
        assert_eq!(s.power.conn, ConnState::Disconnected);
        assert!(s.power.psu.as_ref().is_some_and(|p| p.v_set.is_some()));
    }

    #[test]
    fn late_answer_is_not_taken_for_the_next_query() {
        let dev = SpmDevice::shared();
        let mut link = ScpiLink::new(FakeSpm::new(dev.clone()));
        link.timeout = QUERY_TIMEOUT;
        // busy after a state change: the first answer comes after the retry
        dev.lock().unwrap().late_replies = 1;
        assert!(parse_all_info(&link.query("MEAS:ALL:INFO?").unwrap()).is_some());
        assert_eq!(link.query("CURR?").unwrap(), "1.000");
        assert_eq!(link.query("VOLT:LIM?").unwrap(), "61.000");
    }

    #[test]
    fn end_to_end_late_answers_never_become_set_points() {
        let rig = start(SpmConfig::default());
        wait_for(&rig.store, 3.0, "connect", |s| s.power.psu_conn.is_connected());
        let until = Instant::now() + Duration::from_millis(2500);
        let mut next_late = Instant::now();
        while Instant::now() < until {
            if Instant::now() >= next_late {
                rig.dev.lock().unwrap().late_replies = 1;
                next_late += Duration::from_millis(350);
            }
            if let Some(p) = rig.store.lock().unwrap().power.psu.as_ref() {
                // (`None` only while (re)connecting)
                let is = |x: Option<f64>, want: f64| x.is_none_or(|x| x == want);
                assert!(is(p.v_set, 19.0) && is(p.i_set, 1.0), "set points from a measurement: {p:?}");
                assert!(is(p.ovp, 61.0) && is(p.ocp, 10.5), "thresholds from a measurement: {p:?}");
            }
            std::thread::sleep(Duration::from_millis(3));
        }
        assert!(rig.store.lock().unwrap().power.psu_conn.is_connected());
    }

    #[test]
    fn unknown_model_keeps_full_range() {
        let dev = SpmDevice::shared();
        {
            let mut d = dev.lock().unwrap();
            d.idn = "multicomp pro,MP711134,25280364,FV:V2.0.0";
            d.output_on = false;
            d.ovp = 5.5;
            d.ocp = 0.5;
        }
        let rig = start_on(SpmConfig::default(), dev);
        wait_for(&rig.store, 3.0, "connect", |s| s.power.psu.as_ref().is_some_and(|p| p.ovp == Some(5.5)));
        {
            let s = rig.store.lock().unwrap();
            let p = s.power.psu.as_ref().unwrap();
            // OVP/OCP are the user's settings, not what the model can do
            assert_eq!((p.v_max, p.i_max), (60.0, 10.0));
        }
        rig.psu.send(PsuCommand::SetOvp(13.0)).unwrap();
        rig.psu.send(PsuCommand::SetVoltage(12.0)).unwrap();
        wait_for(&rig.store, 2.0, "read-back", |s| {
            s.power.psu.as_ref().is_some_and(|p| p.ovp == Some(13.0) && p.v_set == Some(12.0))
        });
    }

    #[test]
    fn output_off_survives_a_reconnect_but_nothing_else_does() {
        let rig = start(SpmConfig::default());
        wait_for(&rig.store, 3.0, "connect", |s| s.power.psu_conn.is_connected());
        rig.dev.lock().unwrap().drop_replies = 1_000;
        wait_for(&rig.store, 4.0, "error", |s| matches!(s.power.psu_conn, ConnState::Error(_)));
        // the user clicks around while the supply is unreachable
        for cmd in [PsuCommand::SetVoltage(5.0), PsuCommand::Output(true), PsuCommand::Output(false)] {
            rig.psu.send(cmd).unwrap();
        }
        rig.dev.lock().unwrap().drop_replies = 0;
        wait_for(&rig.store, 4.0, "reconnect", |s| s.power.psu.as_ref().is_some_and(|p| p.output_on == Some(false)));
        let d = rig.dev.lock().unwrap();
        assert!(!d.output_on);
        assert!(d.log.iter().any(|c| c == "OUTP OFF"));
        assert!(!d.log.iter().any(|c| c == "OUTP ON" || c.starts_with("VOLT 5")));
    }

    #[test]
    fn probe_is_read_only() {
        let dev = SpmDevice::shared();
        let d = dev.clone();
        let mut out = Vec::new();
        probe(115_200, move |_| Ok(FakeSpm::new(d.clone())), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("OWON,SPM6103"), "{text}");
        assert!(text.contains("verschiedene Messwerte"));
        assert!(dev.lock().unwrap().log.iter().all(|c| c.ends_with('?')));
    }
}
