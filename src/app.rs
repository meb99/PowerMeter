//! The main window: connection bar, big readouts, live graphs, event list.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, Vec2b};
use egui_plot::{GridMark, Line, Plot, PlotPoints, Span, VLine};
use serde::{Deserialize, Serialize};

use crate::devices::owon_spm::protection_limit as ovp_limit;
use crate::devices::{self, DeviceHandle, DmmCommand, DmmKind, DmmRate, PowerSourceKind, PsuCommand};
use crate::format;
use crate::model::{
    AnalysisSettings, Calibration, ConnState, DmmFunction, EventKind, PowerSample, PsuMode, PsuState, RegMode, Shared,
    Store, decimate, first_index_after,
};
use crate::overlay::OverlayServer;

const COL_V: Color32 = Color32::from_rgb(0xff, 0xd6, 0x0a);
const COL_I: Color32 = Color32::from_rgb(0x4c, 0xc9, 0xf0);
const COL_P: Color32 = Color32::from_rgb(0xff, 0x9f, 0x0a);
const COL_DMM: Color32 = Color32::from_rgb(0xb3, 0x88, 0xff);
const COL_CV: Color32 = Color32::from_rgb(0x30, 0xd1, 0x58);
const COL_CC: Color32 = Color32::from_rgb(0xff, 0x45, 0x3a);
const COL_MUTED: Color32 = Color32::from_rgb(0x8e, 0x8e, 0x93);
const COL_PROT: Color32 = Color32::from_rgb(0xbf, 0x5a, 0xf2);

/// Quick-fill buttons for the voltage field (fill only, never send).
const QUICK_VOLTS: [(f64, &str); 5] = [(3.3, "3,3 V"), (5.0, "5 V"), (12.0, "12 V"), (19.0, "19 V"), (20.0, "20 V")];
/// After an output click the button shows the commanded state this long,
/// since the supply's own report lags behind.
const OUTPUT_PENDING: Duration = Duration::from_millis(1500);
/// A lowered OVP/OCP waits at most this long for the output to drop below it.
const DEFERRED_TIMEOUT: Duration = Duration::from_secs(10);

const WINDOWS: [(f64, &str); 7] = [
    (5.0, "5 s"),
    (10.0, "10 s"),
    (30.0, "30 s"),
    (60.0, "1 min"),
    (300.0, "5 min"),
    (1800.0, "30 min"),
    (f64::INFINITY, "Alles"),
];

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub power_kind: PowerSourceKind,
    pub power_port: String,
    pub power_baud: u32,
    pub dmm_kind: DmmKind,
    pub dmm_port: String,
    pub dmm_baud: u32,
    pub dmm_rate: DmmRate,
    /// Serial port of an OWON SPM (`power_port` stays the box's).
    pub spm_port: String,
    pub spm_baud: u32,
    /// Hybrid: V/I samples from the PowerMon box, the SPM delivers set
    /// points, CV/CC, control and its multimeter.
    pub spm_use_box: bool,
    pub spm_box_port: String,
    /// `SYST:REM` locks the front panel of the SPM.
    pub spm_lock_panel: bool,
    /// The app never sets more than this voltage on a controllable supply.
    pub psu_v_guard: Option<f64>,
    /// Significant digits on the multimeter readout (XDM1241: 55 000 counts).
    pub dmm_digits: usize,
    pub analysis: AnalysisSettings,
    pub calibration: Calibration,
    pub display_avg_ms: f64,
    pub window_secs: f64,
    pub show_v: bool,
    pub show_i: bool,
    pub show_p: bool,
    pub show_dmm_plot: bool,
    pub overlay_port: u16,
    pub overlay_lan: bool,
    pub always_on_top: bool,
    pub auto_connect: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            power_kind: PowerSourceKind::Simulator,
            power_port: String::new(),
            power_baud: 115_200,
            dmm_kind: DmmKind::Simulator,
            dmm_port: String::new(),
            dmm_baud: 115_200,
            dmm_rate: DmmRate::Fast,
            spm_port: String::new(),
            spm_baud: 115_200,
            spm_use_box: false,
            spm_box_port: String::new(),
            spm_lock_panel: false,
            psu_v_guard: None,
            dmm_digits: 5,
            analysis: AnalysisSettings::default(),
            calibration: Calibration::default(),
            display_avg_ms: 100.0,
            window_secs: 30.0,
            show_v: true,
            show_i: true,
            show_p: true,
            show_dmm_plot: true,
            overlay_port: 8765,
            overlay_lan: false,
            always_on_top: false,
            auto_connect: true,
        }
    }
}

pub struct PowerMeterApp {
    store: Shared,
    settings: Settings,
    power_dev: Option<DeviceHandle>,
    /// PowerMon box delivering the samples in the hybrid SPM setup.
    box_dev: Option<DeviceHandle>,
    dmm_dev: Option<DeviceHandle>,
    /// The user wants the SPM's built-in multimeter running.
    spm_dmm: bool,
    /// `power_dev` is an SPM thread that also runs its multimeter.
    power_dev_with_dmm: bool,
    /// U, I, OVP, OCP as typed but not yet sent; `None` follows the supply.
    psu_edit: [Option<f64>; 4],
    /// Lowered OVP (2) / OCP (3) from "Übernehmen", sent once the output is
    /// below it: (index into `psu_edit`, value, since).
    psu_deferred: Vec<(usize, f64, Instant)>,
    /// Last output command from a click or key O, and when.
    output_cmd: Option<(bool, Instant)>,
    overlay: Option<OverlayServer>,
    ports: Vec<String>,
    last_port_scan: Instant,
    /// `Some` while the graphs are frozen for inspection.
    paused: bool,
    view_x: (f64, f64),
    toast: Option<(String, Instant)>,
    show_settings: bool,
}

impl PowerMeterApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let settings: Settings = cc.storage.and_then(|s| eframe::get_value(s, "settings")).unwrap_or_default();
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let store = Store::shared();
        store.lock().unwrap().analysis = settings.analysis.clone();
        store.lock().unwrap().analysis.display_avg_ms = settings.display_avg_ms;
        store.lock().unwrap().calibration = settings.calibration;
        if settings.always_on_top {
            cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
        }
        let mut app = Self {
            store,
            settings,
            power_dev: None,
            box_dev: None,
            dmm_dev: None,
            spm_dmm: false,
            power_dev_with_dmm: false,
            psu_edit: [None; 4],
            psu_deferred: Vec::new(),
            output_cmd: None,
            overlay: None,
            ports: devices::list_ports(),
            last_port_scan: Instant::now(),
            paused: false,
            view_x: (0.0, 30.0),
            toast: None,
            show_settings: false,
        };
        app.restart_overlay();
        // Connecting only reads: no set command goes to a supply here.
        if app.settings.auto_connect {
            app.spm_dmm = app.settings.dmm_kind == DmmKind::OwonSpm;
            if app.power_port_chosen() {
                app.connect_power(&cc.egui_ctx);
            }
            if app.settings.dmm_kind != DmmKind::OwonXdm || !app.settings.dmm_port.is_empty() {
                app.connect_dmm(&cc.egui_ctx);
            }
        }
        app
    }

    fn power_port_chosen(&self) -> bool {
        match self.settings.power_kind {
            PowerSourceKind::PowerMon => !self.settings.power_port.is_empty(),
            PowerSourceKind::OwonSpm => !self.settings.spm_port.is_empty(),
            PowerSourceKind::SpmSimulator | PowerSourceKind::Simulator => true,
        }
    }

    fn power_running(&self) -> bool {
        self.power_dev.as_ref().is_some_and(|d| !d.is_finished())
    }

    fn dmm_running(&self) -> bool {
        self.dmm_dev.as_ref().is_some_and(|d| !d.is_finished()) || (self.power_dev_with_dmm && self.power_running())
    }

    /// The running OWON SPM thread, if any.
    fn psu_handle(&self) -> Option<&DeviceHandle> {
        self.power_dev.as_ref().filter(|d| d.psu.is_some() && !d.is_finished())
    }

    /// Where multimeter commands go: the SPM thread for the built-in meter.
    fn dmm_handle(&self) -> Option<&DeviceHandle> {
        if self.settings.dmm_kind == DmmKind::OwonSpm {
            self.power_dev.as_ref().filter(|_| self.power_dev_with_dmm)
        } else {
            self.dmm_dev.as_ref()
        }
    }

    fn send_psu(&mut self, cmd: PsuCommand) -> bool {
        let sent = self.psu_handle().is_some_and(|h| h.send_psu(cmd));
        if !sent {
            self.toast("Netzteil nicht verbunden");
        }
        if let (true, PsuCommand::Output(on)) = (sent, cmd) {
            self.output_cmd = Some((on, Instant::now()));
        }
        sent
    }

    fn overlay_url(&self) -> String {
        format!("http://127.0.0.1:{}/overlay", self.settings.overlay_port)
    }

    fn restart_overlay(&mut self) {
        self.overlay = None; // stop the old one first so the port is free
        let ip =
            if self.settings.overlay_lan { IpAddr::V4(Ipv4Addr::UNSPECIFIED) } else { IpAddr::V4(Ipv4Addr::LOCALHOST) };
        let addr = SocketAddr::new(ip, self.settings.overlay_port);
        self.overlay = Some(OverlayServer::start(self.store.clone(), addr, self.settings.dmm_digits));
    }

    fn disconnect_power(&mut self) {
        self.power_dev = None;
        self.box_dev = None;
        self.power_dev_with_dmm = false;
        self.psu_edit = [None; 4];
        self.psu_deferred.clear();
        self.output_cmd = None;
    }

    fn connect_power(&mut self, ctx: &egui::Context) {
        self.disconnect_power();
        let kind = self.settings.power_kind;
        let store = self.store.clone();
        let ctx2 = ctx.clone();
        let handle = match kind {
            PowerSourceKind::Simulator => {
                DeviceHandle::spawn("power-sim", move |stop| devices::sim::run_power(store, ctx2, 100.0, stop))
            }
            PowerSourceKind::PowerMon => {
                let cfg = devices::powermon::PowerMonConfig {
                    port: self.settings.power_port.clone(),
                    baud: self.settings.power_baud,
                };
                DeviceHandle::spawn("powermon", move |stop| devices::powermon::run(cfg, store, ctx2, stop))
            }
            PowerSourceKind::OwonSpm | PowerSourceKind::SpmSimulator => {
                let with_dmm = self.spm_dmm && self.settings.dmm_kind == DmmKind::OwonSpm;
                let hybrid = kind == PowerSourceKind::OwonSpm && self.settings.spm_use_box;
                let cfg = devices::owon_spm::SpmConfig {
                    port: self.settings.spm_port.clone(),
                    baud: self.settings.spm_baud,
                    with_dmm,
                    push_samples: !hybrid,
                    lock_panel: self.settings.spm_lock_panel,
                    ..Default::default()
                };
                let (psu_tx, psu_rx) = mpsc::channel();
                let (dmm_tx, dmm_rx) = mpsc::channel();
                let mut h = if kind == PowerSourceKind::SpmSimulator {
                    DeviceHandle::spawn("spm-sim", move |stop| {
                        devices::sim::run_spm(cfg, store, ctx2, psu_rx, dmm_rx, stop)
                    })
                } else {
                    DeviceHandle::spawn("owon-spm", move |stop| {
                        devices::owon_spm::run(cfg, store, ctx2, psu_rx, dmm_rx, stop)
                    })
                };
                h.psu = Some(psu_tx);
                h.commands = Some(dmm_tx);
                self.power_dev_with_dmm = with_dmm;
                if hybrid {
                    self.connect_box(ctx);
                }
                h
            }
        };
        self.power_dev = Some(handle);
        if self.settings.dmm_kind == DmmKind::OwonSpm && !kind.is_spm() {
            self.store.lock().unwrap().dmm.conn = ConnState::Error("Netzteil ist kein OWON SPM".into());
        }
    }

    /// The PowerMon box next to an SPM (hybrid setup).
    fn connect_box(&mut self, ctx: &egui::Context) {
        self.box_dev = None;
        if self.settings.spm_box_port.is_empty() {
            self.store.lock().unwrap().power.conn = ConnState::Error("PowerMon-Box: kein Port gewählt".into());
            return;
        }
        let cfg = devices::powermon::PowerMonConfig {
            port: self.settings.spm_box_port.clone(),
            baud: self.settings.power_baud,
        };
        let store = self.store.clone();
        let ctx = ctx.clone();
        self.box_dev =
            Some(DeviceHandle::spawn("powermon-box", move |stop| devices::powermon::run(cfg, store, ctx, stop)));
    }

    fn disconnect_dmm(&mut self, ctx: &egui::Context) {
        self.dmm_dev = None;
        if self.spm_dmm {
            self.spm_dmm = false;
            if self.power_dev_with_dmm && self.power_running() {
                // Restart the SPM thread without its multimeter.
                self.connect_power(ctx);
            }
        }
    }

    fn connect_dmm(&mut self, ctx: &egui::Context) {
        self.dmm_dev = None;
        if self.settings.dmm_kind == DmmKind::OwonSpm {
            // The built-in meter runs on the supply's connection.
            self.spm_dmm = true;
            let error = if !self.settings.power_kind.is_spm() {
                Some("Netzteil ist kein OWON SPM")
            } else if !self.power_running() {
                Some("Erst das Netzteil verbinden")
            } else {
                None
            };
            match error {
                Some(e) => self.store.lock().unwrap().dmm.conn = ConnState::Error(e.into()),
                None if !self.power_dev_with_dmm => self.connect_power(ctx),
                None => {}
            }
            return;
        }
        // Leaving the built-in meter: the SPM thread drops it.
        self.disconnect_dmm(ctx);
        let store = self.store.clone();
        let ctx = ctx.clone();
        let (tx, rx) = mpsc::channel();
        let mut handle = match self.settings.dmm_kind {
            DmmKind::Simulator => {
                DeviceHandle::spawn("dmm-sim", move |stop| devices::sim::run_dmm(store, ctx, rx, stop))
            }
            DmmKind::OwonSpm => return, // handled above
            DmmKind::OwonXdm => {
                let cfg = devices::owon::OwonConfig {
                    port: self.settings.dmm_port.clone(),
                    baud: self.settings.dmm_baud,
                    rate: self.settings.dmm_rate,
                };
                DeviceHandle::spawn("owon", move |stop| devices::owon::run(cfg, store, ctx, rx, stop))
            }
        };
        handle.commands = Some(tx);
        handle.send(DmmCommand::SetRate(self.settings.dmm_rate));
        self.dmm_dev = Some(handle);
    }

    fn sync_analysis(&mut self) {
        let mut s = self.store.lock().unwrap();
        s.analysis = self.settings.analysis.clone();
        s.analysis.display_avg_ms = self.settings.display_avg_ms;
        s.calibration = self.settings.calibration;
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn export(&mut self) {
        match crate::export::export_session(&self.store) {
            Ok(dir) => {
                self.toast(format!("Gespeichert: {}", dir.display()));
                #[cfg(target_os = "macos")]
                let _ = std::process::Command::new("open").arg(&dir).spawn();
            }
            Err(e) => self.toast(format!("Export fehlgeschlagen: {e}")),
        }
    }

    // ---------------------------------------------------------------- top bar

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        if self.last_port_scan.elapsed() > Duration::from_secs(2) {
            self.ports = devices::list_ports();
            self.last_port_scan = Instant::now();
        }
        let ctx = ui.ctx().clone();
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("⚡ PowerMeter").strong().size(18.0));
            ui.separator();

            // power source
            let (conn, psu_conn) = {
                let s = self.store.lock().unwrap();
                (s.power.conn.clone(), s.power.psu_conn.clone())
            };
            ui.label(RichText::new("Netzteil").strong());
            let before = self.settings.power_kind;
            let ports_before = (self.settings.power_port.clone(), self.settings.spm_port.clone());
            egui::ComboBox::from_id_salt("power_kind").selected_text(self.settings.power_kind.label()).show_ui(
                ui,
                |ui| {
                    for k in PowerSourceKind::ALL {
                        ui.selectable_value(&mut self.settings.power_kind, k, k.label());
                    }
                },
            );
            match self.settings.power_kind {
                PowerSourceKind::PowerMon => port_combo(ui, "power_port", &mut self.settings.power_port, &self.ports),
                PowerSourceKind::OwonSpm => port_combo(ui, "spm_port", &mut self.settings.spm_port, &self.ports),
                PowerSourceKind::SpmSimulator | PowerSourceKind::Simulator => {}
            }
            let changed = before != self.settings.power_kind
                || ports_before != (self.settings.power_port.clone(), self.settings.spm_port.clone());
            if self.power_running() && !changed {
                if ui.button("Trennen").clicked() {
                    self.disconnect_power();
                }
            } else if ui.button("Verbinden").clicked() || (changed && self.power_dev.is_some()) {
                // Without a port the thread would retry "" forever.
                if self.power_port_chosen() {
                    self.connect_power(&ctx);
                } else {
                    self.disconnect_power();
                    self.toast("Erst den Port wählen, dann Verbinden");
                }
            }
            if self.settings.power_kind.is_spm() {
                conn_dot(ui, &psu_conn);
                if self.box_dev.is_some()
                    || (self.settings.power_kind == PowerSourceKind::OwonSpm && self.settings.spm_use_box)
                {
                    ui.label(RichText::new("Box").small().color(COL_MUTED));
                    conn_dot(ui, &conn);
                }
            } else {
                conn_dot(ui, &conn);
            }

            ui.separator();

            // multimeter
            let conn = self.store.lock().unwrap().dmm.conn.clone();
            ui.label(RichText::new("Multimeter").strong());
            let before = self.settings.dmm_kind;
            let was_running = self.dmm_running();
            egui::ComboBox::from_id_salt("dmm_kind").selected_text(self.settings.dmm_kind.label()).show_ui(ui, |ui| {
                for k in DmmKind::ALL {
                    ui.selectable_value(&mut self.settings.dmm_kind, k, k.label());
                }
            });
            match self.settings.dmm_kind {
                DmmKind::OwonXdm => port_combo(ui, "dmm_port", &mut self.settings.dmm_port, &self.ports),
                DmmKind::OwonSpm => {
                    ui.label(RichText::new("über Netzteil").color(COL_MUTED));
                }
                DmmKind::Simulator => {}
            }
            let changed = before != self.settings.dmm_kind;
            if was_running && !changed {
                if ui.button("Trennen").clicked() {
                    self.disconnect_dmm(&ctx);
                }
            } else if ui.button("Verbinden").clicked() || (changed && was_running) {
                self.connect_dmm(&ctx);
            }
            conn_dot(ui, &conn);

            ui.separator();
            let url = self.overlay_url();
            match self.overlay.as_ref().and_then(|o| o.error.clone()) {
                Some(err) => {
                    ui.colored_label(COL_CC, format!("Overlay: {err}"));
                }
                None => {
                    ui.label(RichText::new("OBS").strong());
                    if ui.link(&url).on_hover_text("Im Browser öffnen").clicked() {
                        ctx.open_url(egui::OpenUrl::new_tab(format!(
                            "http://127.0.0.1:{}/",
                            self.settings.overlay_port
                        )));
                    }
                    if ui.small_button("📋").on_hover_text("URL kopieren (OBS → Browser-Quelle)").clicked() {
                        ctx.copy_text(url.clone());
                        self.toast("Overlay-URL kopiert");
                    }
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.toggle_value(&mut self.show_settings, "⚙ Einstellungen");
            });
        });
    }

    // --------------------------------------------------------------- readouts

    fn readouts(&mut self, ui: &mut egui::Ui) {
        let (disp, mode, v_set, i_set, dmm, dmm_fn, dmm_conn, power_conn, short, dip, prot) = {
            let s = self.store.lock().unwrap();
            let a = &s.power.analyzer;
            let now = s.now();
            let disp = s.display_power().filter(|p| now - p.t < 2.0);
            let dmm = s.dmm.samples.back().filter(|d| now - d.t < 3.0).map(|d| d.value);
            let approx = |learned: bool| if learned { "≈ " } else { "" };
            (
                disp,
                a.mode,
                a.v_set_effective(&s.analysis).map(|v| (v, approx(a.v_set_is_learned(&s.analysis)))),
                a.i_limit(&s.analysis).map(|i| (i, approx(a.i_set_is_learned(&s.analysis)))),
                dmm,
                s.dmm.function,
                s.dmm.conn.is_connected(),
                s.power.conn.is_connected(),
                a.active(EventKind::Short),
                a.active(EventKind::Dropout),
                a.active(EventKind::Protection),
            )
        };
        let avail = ui.available_width();
        let cols = if dmm_conn { 4.0 } else { 3.0 };
        let w = ((avail - (cols - 1.0) * 8.0) / cols).max(150.0);
        let size = ((w - 28.0) / 5.4).clamp(24.0, 72.0);

        ui.horizontal(|ui| {
            let sub_v = v_set.map(|(v, approx)| format!("{approx}Soll {} V", format::fixed(v, 2)));
            let sub_i = i_set.map(|(i, approx)| format!("{approx}Limit {} A", format::fixed(i, 3)));
            let alarm = if short {
                Some(("KURZSCHLUSS", COL_CC))
            } else if prot {
                Some(("SCHUTZ", COL_PROT))
            } else if dip {
                Some(("EINBRUCH", COL_CC))
            } else {
                None
            };
            let badge = power_conn.then(|| {
                let c = match mode {
                    RegMode::Cv => COL_CV,
                    RegMode::Cc => COL_CC,
                    _ => COL_MUTED,
                };
                (mode.label(), c)
            });
            readout(
                ui,
                w,
                size,
                "SPANNUNG",
                disp.map_or("--.---".into(), |p| format::fixed(p.v, 3)),
                "V",
                COL_V,
                sub_v,
                badge.or(alarm),
            );
            readout(
                ui,
                w,
                size,
                "STROM",
                disp.map_or("-.----".into(), |p| format::fixed(p.i, 4)),
                "A",
                COL_I,
                sub_i,
                alarm.filter(|_| badge.is_some()),
            );
            readout(
                ui,
                w,
                size,
                "LEISTUNG",
                disp.map_or("--.--".into(), |p| format::fixed(p.p(), if p.p() >= 100.0 { 1 } else { 3 })),
                "W",
                COL_P,
                None,
                None,
            );
            if dmm_conn {
                let (text, unit) = match dmm {
                    None => ("-----".to_string(), dmm_fn.unit().to_string()),
                    Some(v) if v.is_nan() => ("OL".to_string(), dmm_fn.unit().to_string()),
                    Some(v) => {
                        let (m, p) = format::scale(v);
                        (format::sig(m, self.settings.dmm_digits), format!("{p}{}", dmm_fn.unit()))
                    }
                };
                let label = format!("MULTIMETER · {}", dmm_fn.label().to_uppercase());
                readout(ui, w, size, &label, text, &unit, COL_DMM, None, None);
            }
        });
    }

    // ------------------------------------------------------------------ plots

    fn plots(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Zeitfenster:");
            for (secs, label) in WINDOWS {
                if ui.selectable_label(self.settings.window_secs == secs, label).clicked() {
                    self.settings.window_secs = secs;
                    self.paused = false;
                }
            }
            ui.separator();
            let pause_label = if self.paused { "▶ Live" } else { "⏸ Pause" };
            if ui.button(pause_label).on_hover_text("Leertaste").clicked() {
                self.paused = !self.paused;
            }
            if ui.button("📍 Marker").on_hover_text("M – markiert den Moment (für den Videoschnitt)").clicked() {
                self.store.lock().unwrap().add_marker();
            }
            if ui.button("↺ Statistik").on_hover_text("R – Energie, Min/Max zurücksetzen").clicked() {
                self.store.lock().unwrap().power.analyzer.reset_stats();
            }
            if ui.button("🗑 Leeren").on_hover_text("Alle Messwerte und Ereignisse löschen").clicked() {
                self.store.lock().unwrap().clear_all();
            }
            if ui.button("💾 CSV").on_hover_text("Sitzung als CSV nach ~/Documents/PowerMeter exportieren").clicked()
            {
                self.export();
            }
            ui.separator();
            ui.checkbox(&mut self.settings.show_v, RichText::new("V").color(COL_V));
            ui.checkbox(&mut self.settings.show_i, RichText::new("A").color(COL_I));
            ui.checkbox(&mut self.settings.show_p, RichText::new("W").color(COL_P));
            ui.checkbox(&mut self.settings.show_dmm_plot, RichText::new("DMM").color(COL_DMM));
        });

        let px = ui.available_width().max(200.0) as usize;
        let max_points = px * 2;

        // Copy what we need out of the store, then draw without holding the lock.
        let (now, power_series, dmm_series, spans, markers, v_set, i_set, dmm_unit) = {
            let s = self.store.lock().unwrap();
            let now = s.now();
            let (x0, x1) = if self.paused {
                self.view_x
            } else if self.settings.window_secs.is_finite() {
                (now - self.settings.window_secs, now)
            } else {
                (s.power.samples.front().map_or(0.0, |p| p.t).min(s.dmm.samples.front().map_or(now, |d| d.t)), now)
            };
            // one sample of margin on each side so lines reach the edges
            let start = first_index_after(&s.power.samples, x0, |p| p.t).saturating_sub(1);
            let end = (first_index_after(&s.power.samples, x1, |p| p.t) + 1).min(s.power.samples.len());
            let slice: Vec<PowerSample> = s.power.samples.range(start..end).copied().collect();
            let v = decimate(slice.iter().map(|p| [p.t, p.v]), max_points);
            let i = decimate(slice.iter().map(|p| [p.t, p.i]), max_points);
            let p = decimate(slice.iter().map(|p| [p.t, p.p()]), max_points);
            let ds = first_index_after(&s.dmm.samples, x0, |d| d.t).saturating_sub(1);
            let de = (first_index_after(&s.dmm.samples, x1, |d| d.t) + 1).min(s.dmm.samples.len());
            let dmm: Vec<[f64; 2]> = decimate(s.dmm.samples.range(ds..de).map(|d| [d.t, d.value]), max_points);
            let a = &s.power.analyzer;
            let spans: Vec<(EventKind, f64, f64)> = a
                .events
                .iter()
                .filter(|e| e.kind != EventKind::Marker && e.t_end.unwrap_or(now) >= x0 && e.t_start <= x1)
                .map(|e| (e.kind, e.t_start, e.t_end.unwrap_or(now).max(e.t_start + 0.005)))
                .collect();
            let markers: Vec<f64> = a
                .events
                .iter()
                .filter(|e| e.kind == EventKind::Marker && e.t_start >= x0 && e.t_start <= x1)
                .map(|e| e.t_start)
                .collect();
            (
                now,
                (v, i, p),
                dmm,
                spans,
                markers,
                a.v_set_effective(&s.analysis),
                a.i_limit(&s.analysis),
                s.dmm.function.unit(),
            )
        };

        let live = !self.paused;
        let x_range = if live {
            let w = if self.settings.window_secs.is_finite() { self.settings.window_secs } else { now.max(1.0) };
            Some((now - w).max(if self.settings.window_secs.is_finite() { f64::NEG_INFINITY } else { 0.0 })..=now)
        } else {
            None
        };
        let link = egui::Id::new("time_axis");
        let enabled: Vec<PlotRow> = [
            (self.settings.show_v, ("Spannung", "V", COL_V, &power_series.0, v_set)),
            (self.settings.show_i, ("Strom", "A", COL_I, &power_series.1, i_set)),
            (self.settings.show_p, ("Leistung", "W", COL_P, &power_series.2, None)),
        ]
        .into_iter()
        .filter(|(on, _)| *on)
        .map(|(_, x)| x)
        .collect();
        let dmm_rows = usize::from(self.settings.show_dmm_plot && !dmm_series.is_empty());
        let rows = enabled.len() + dmm_rows;
        if rows == 0 {
            return;
        }
        let gap = 6.0;
        let h = ((ui.available_height() - gap * rows as f32) / rows as f32).max(80.0);

        let mut view = None;
        let make_plot = |id: &str, unit: &str| {
            let unit = unit.to_string();
            Plot::new(id.to_string())
                .height(h)
                .link_axis(link, Vec2b::new(true, false))
                .link_cursor(link, Vec2b::new(true, false))
                .allow_drag(!live)
                .allow_zoom(!live)
                .allow_scroll(!live)
                .allow_boxed_zoom(!live)
                .y_axis_min_width(64.0)
                .x_axis_formatter(|m: GridMark, _| clock(m.value))
                .y_axis_formatter(move |m: GridMark, _| format::si(m.value, &unit, 3))
        };

        for (name, unit, color, series, setpoint) in enabled {
            let plot = make_plot(name, unit).include_y(0.0);
            let unit_s = unit.to_string();
            let resp = plot
                .label_formatter(move |hp| {
                    let pos = match hp {
                        egui_plot::HoverPosition::NearDataPoint { position, .. } => position,
                        egui_plot::HoverPosition::Elsewhere { position } => position,
                    };
                    Some(format!("{}\n{}", clock(pos.x), format::si(pos.y, &unit_s, 5)))
                })
                .show(ui, |pui| {
                    if let Some(r) = x_range.clone() {
                        pui.set_plot_bounds_x(r);
                        pui.set_auto_bounds(Vec2b::new(false, true));
                    }
                    for (kind, a, b) in &spans {
                        let (c, label) = match kind {
                            EventKind::Dropout => (Color32::from_rgba_unmultiplied(255, 69, 58, 50), "Einbruch"),
                            EventKind::Short => (Color32::from_rgba_unmultiplied(255, 0, 80, 80), "Kurzschluss"),
                            EventKind::Protection => {
                                (Color32::from_rgba_unmultiplied(191, 90, 242, 50), "Schutzabschaltung")
                            }
                            EventKind::CurrentLimit | EventKind::Marker => {
                                (Color32::from_rgba_unmultiplied(255, 159, 10, 30), "CC")
                            }
                        };
                        pui.span(Span::new(label, *a..=*b).fill(c).border_width(0.0));
                    }
                    for m in &markers {
                        pui.vline(VLine::new("Marker", *m).color(Color32::WHITE).width(1.0));
                    }
                    if let Some(sp) = setpoint {
                        pui.hline(
                            egui_plot::HLine::new(format!("Soll {name}"), sp)
                                .color(color.gamma_multiply(0.4))
                                .width(1.0),
                        );
                    }
                    pui.line(Line::new(name, PlotPoints::new(series.clone())).color(color).width(1.6));
                });
            view.get_or_insert(resp.transform.bounds().range_x());
            ui.add_space(gap);
        }
        if dmm_rows == 1 {
            let resp = make_plot("Multimeter", dmm_unit).show(ui, |pui| {
                if let Some(r) = x_range.clone() {
                    pui.set_plot_bounds_x(r);
                    pui.set_auto_bounds(Vec2b::new(false, true));
                }
                for m in &markers {
                    pui.vline(VLine::new("Marker", *m).color(Color32::WHITE).width(1.0));
                }
                pui.line(Line::new("Multimeter", PlotPoints::new(dmm_series)).color(COL_DMM).width(1.6));
            });
            view.get_or_insert(resp.transform.bounds().range_x());
        }
        if let Some(r) = view {
            self.view_x = (*r.start(), *r.end());
        }
    }

    // ------------------------------------------------------------- side panel

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("Netzteil-Einstellung");
            let psu = if self.settings.power_kind.is_spm() && self.psu_handle().is_some() {
                self.store.lock().unwrap().power.psu.clone()
            } else {
                None
            };
            let mut changed = false;
            match psu {
                Some(psu) => self.psu_controls(ui, &psu),
                None => changed |= self.manual_setpoints(ui),
            }

            ui.separator();
            ui.heading("Statistik");
            self.stats(ui);

            ui.separator();
            ui.heading("Multimeter");
            self.dmm_controls(ui);

            ui.separator();
            ui.heading("Ereignisse");
            self.events(ui);

            if changed {
                self.sync_analysis();
            }
        });
    }

    /// Set points typed in or learned, for a supply without interface.
    /// Returns true when the analysis settings changed.
    fn manual_setpoints(&mut self, ui: &mut egui::Ui) -> bool {
        let note = if self.settings.power_kind.is_spm() {
            "OWON SPM nicht verbunden – bis dahin gelten diese Werte."
        } else {
            "Das Netzteil hat keine Schnittstelle – Sollwerte hier eintragen oder automatisch lernen lassen."
        };
        ui.label(RichText::new(note).small().color(COL_MUTED));
        let (learned_v, learned_i) = {
            let s = self.store.lock().unwrap();
            (s.power.analyzer.learned_vset, s.power.analyzer.learned_iset)
        };
        let mut changed = false;
        egui::Grid::new("setpoints").num_columns(2).show(ui, |ui| {
            ui.label("Spannung (Soll)");
            changed |= optional_value(ui, &mut self.settings.analysis.v_set, 19.0, " V", 0.01, learned_v);
            ui.end_row();
            ui.label("Strombegrenzung");
            changed |= optional_value(ui, &mut self.settings.analysis.i_set, 1.0, " A", 0.001, learned_i);
            ui.end_row();
        });
        changed |=
            ui.checkbox(&mut self.settings.analysis.auto_learn_vset, "Sollspannung im Leerlauf lernen").changed();
        changed |= ui
            .checkbox(&mut self.settings.analysis.auto_learn_iset, "Strombegrenzung bei Kurzschluss lernen")
            .changed();
        changed
    }

    /// Highest voltage the app may set: model limit, capped by the user's
    /// own ceiling.
    fn v_ceiling(&self, psu: &PsuState) -> f64 {
        self.settings.psu_v_guard.map_or(psu.v_max, |g| g.min(psu.v_max))
    }

    /// Controls for an OWON SPM. Values go to the supply only on
    /// "Übernehmen" or Enter – never while dragging.
    fn psu_controls(&mut self, ui: &mut egui::Ui, psu: &PsuState) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(&psu.model).strong());
            let c = match psu.mode {
                PsuMode::Cv => COL_CV,
                PsuMode::Cc | PsuMode::Fault => COL_CC,
                PsuMode::Standby | PsuMode::Unknown => COL_MUTED,
            };
            badge(ui, psu.mode.label(), c);
            if self.settings.spm_lock_panel {
                ui.label(RichText::new("Bedienfeld gesperrt").small().color(COL_MUTED));
            }
        });
        if let Some(p) = psu.protection_text() {
            ui.label(RichText::new(format!("⚠ Schutzabschaltung: {p}")).color(COL_CC).strong());
            ui.label(RichText::new("Ursache prüfen, dann den Ausgang wieder einschalten.").small().color(COL_MUTED));
        }

        let v_cap = self.v_ceiling(psu);
        // The voltage field takes up to the model limit, so "Übernehmen" can
        // say when the guard cuts it down.
        let rows: [(&str, Option<f64>, f64, &str, f64); 4] = [
            ("Spannung (Soll)", psu.v_set, psu.v_max, " V", 0.01),
            ("Strombegrenzung", psu.i_set, psu.i_max, " A", 0.001),
            ("OVP", psu.ovp, ovp_limit(psu.v_max), " V", 0.01),
            ("OCP", psu.ocp, ovp_limit(psu.i_max), " A", 0.001),
        ];
        let mut apply = false;
        egui::Grid::new("psu_set").num_columns(3).show(ui, |ui| {
            for (k, (label, device, max, unit, speed)) in rows.into_iter().enumerate() {
                ui.label(label);
                let mut value = self.psu_edit[k].or(device).unwrap_or(0.0);
                let r = ui.add(
                    egui::DragValue::new(&mut value)
                        .range(0.0..=max)
                        // a read-back outside the range must not count as an edit
                        .clamp_existing_to_range(false)
                        .speed(speed)
                        .suffix(unit)
                        .fixed_decimals(3)
                        .custom_parser(parse_decimal),
                );
                if r.changed() {
                    self.psu_edit[k] = Some(value);
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    apply = true;
                }
                let readback = device.map_or("Gerät: –".to_string(), |d| format!("Gerät: {d:.3}{unit}"));
                let pending = self.psu_edit[k].is_some_and(|e| device.is_none_or(|d| (e - d).abs() > 0.0005));
                ui.label(RichText::new(readback).small().color(if pending { COL_P } else { COL_MUTED }));
                ui.end_row();
            }
        });
        ui.horizontal_wrapped(|ui| {
            for (v, label) in QUICK_VOLTS {
                if ui
                    .small_button(label)
                    .on_hover_text("Trägt den Wert nur ins Feld ein. Erst „Übernehmen“ schickt ihn ans Netzteil.")
                    .clicked()
                {
                    self.psu_edit[0] = Some(v);
                    if v > v_cap + 0.0005 {
                        self.toast(format!(
                            "Über der Spannungsgrenze – wird auf {} V begrenzt",
                            format::fixed(v_cap, 2)
                        ));
                    }
                }
            }
        });
        let dirty = self.psu_edit.iter().any(Option::is_some);
        ui.horizontal(|ui| {
            if ui.add_enabled(dirty, egui::Button::new("Übernehmen")).on_hover_text("Oder Enter im Feld").clicked() {
                apply = true;
            }
            if dirty && ui.button("Verwerfen").clicked() {
                self.psu_edit = [None; 4];
            }
        });
        if apply {
            self.apply_psu_edit(psu);
        }
        for (k, x, _) in &self.psu_deferred {
            let (name, unit) = if *k == 2 { ("OVP", "V") } else { ("OCP", "A") };
            ui.label(
                RichText::new(format!(
                    "{name} {} {unit} folgt, sobald der Ausgang darunter liegt",
                    format::fixed(*x, 3)
                ))
                .small()
                .color(COL_P),
            );
        }

        ui.add_space(6.0);
        // Right after a click the supply's report still shows the old state
        // (and may flip back once): show what was commanded, so a quick
        // second click undoes the first instead of repeating it.
        let commanded = self.output_cmd.filter(|(_, at)| at.elapsed() < OUTPUT_PENDING).map(|(on, _)| on);
        let in_flight = commanded.is_some_and(|on| psu.output_on != Some(on));
        let (text, fill, hover) = match (commanded.or(psu.output_on), in_flight) {
            (Some(true), false) => ("AUSGANG EIN", COL_CV, "Klicken schaltet den Ausgang aus (Taste O)"),
            (Some(true), true) => ("AUSGANG EIN …", COL_CV, "Wird eingeschaltet – Klicken schaltet den Ausgang aus"),
            (Some(false), false) => ("AUSGANG AUS", COL_CC, "Klicken schaltet den Ausgang ein"),
            (Some(false), true) => ("AUSGANG AUS …", COL_CC, "Wird ausgeschaltet – Klicken schaltet ihn wieder ein"),
            (None, _) => ("AUSGANG ?", COL_MUTED, "Zustand unbekannt – Klicken schaltet den Ausgang aus"),
        };
        let button = egui::Button::new(RichText::new(text).size(22.0).strong().color(Color32::BLACK))
            .fill(fill)
            .min_size(egui::vec2(ui.available_width(), 44.0));
        if ui.add(button).on_hover_text(hover).clicked() {
            if commanded.or(psu.output_on) == Some(false) {
                self.switch_output_on(psu);
            } else {
                self.send_psu(PsuCommand::Output(false));
            }
        }
        ui.label(
            RichText::new(
                "Werte gehen erst mit „Übernehmen“ oder Enter ans Netzteil. Taste O schaltet den Ausgang sofort aus.",
            )
            .small()
            .color(COL_MUTED),
        );
        if let Some(note) = &psu.note {
            ui.label(RichText::new(note).small().color(COL_P));
        }
    }

    /// Sends what was typed (see [`plan_psu_edit`] for the order).
    fn apply_psu_edit(&mut self, psu: &PsuState) {
        let plan = plan_psu_edit(std::mem::take(&mut self.psu_edit), psu, self.v_ceiling(psu));
        if !plan.notes.is_empty() {
            self.toast(plan.notes.join(" · "));
        }
        self.psu_deferred = plan.deferred.into_iter().map(|(k, x)| (k, x, Instant::now())).collect();
        for cmd in plan.now {
            self.send_psu(cmd);
        }
        self.flush_deferred_psu();
    }

    /// Sends a lowered OVP/OCP once the output is below it (or off).
    fn flush_deferred_psu(&mut self) {
        if self.psu_deferred.is_empty() {
            return;
        }
        let (psu_off, v_set, out) = {
            let s = self.store.lock().unwrap();
            let out = s.display_power().filter(|p| s.now() - p.t < 1.0);
            let psu = s.power.psu.as_ref();
            (psu.map(|p| p.output_on == Some(false)), psu.and_then(|p| p.v_set), out)
        };
        let Some(output_off) = psu_off else {
            self.psu_deferred.clear(); // supply gone
            return;
        };
        for (k, x, since) in std::mem::take(&mut self.psu_deferred) {
            let below = out.is_some_and(|p| if k == 2 { p.v <= x - 0.02 } else { p.i <= x - 0.002 });
            // A lowered OVP only once the supply has really taken the lower
            // set voltage: a dip (or a rejected VOLT) must not let an OVP at
            // or below the set voltage through, it would trip the output.
            let vset_ok = k != 2 || v_set.is_none_or(|v| x > v + 0.0005);
            if (output_off || below) && vset_ok {
                self.send_psu(psu_command(k, x));
            } else if since.elapsed() > DEFERRED_TIMEOUT {
                let name = if k == 2 { "OVP" } else { "OCP" };
                self.toast(format!(
                    "{name} {} nicht gesetzt: Ausgang oder Sollwert liegt noch darüber",
                    format::fixed(x, 3)
                ));
            } else {
                self.psu_deferred.push((k, x, since));
            }
        }
    }

    /// Only ever called from a click on the output button.
    fn switch_output_on(&mut self, psu: &PsuState) {
        if let (Some(guard), Some(v)) = (self.settings.psu_v_guard, psu.v_set)
            && v > guard + 0.0005
        {
            self.toast(format!(
                "Nicht eingeschaltet: Soll {} V liegt über der Spannungsgrenze {} V",
                format::fixed(v, 2),
                format::fixed(guard, 2)
            ));
            return;
        }
        self.send_psu(PsuCommand::Output(true));
    }

    fn stats(&mut self, ui: &mut egui::Ui) {
        let (a, ripple, dmm_stats, dmm_unit, power_rate, dmm_rate) = {
            let s = self.store.lock().unwrap();
            let now = s.now();
            let power_rate = s.power.samples.iter().rev().take_while(|p| now - p.t <= 1.0).count();
            let dmm_rate = s.dmm.samples.iter().rev().take_while(|p| now - p.t <= 1.0).count();
            (s.power.analyzer.clone(), s.ripple(1.0), s.dmm.stats, s.dmm.function.unit(), power_rate, dmm_rate)
        };
        egui::Grid::new("stats").num_columns(4).striped(true).show(ui, |ui| {
            ui.label("");
            ui.label(RichText::new("Min").color(COL_MUTED));
            ui.label(RichText::new("Ø").color(COL_MUTED));
            ui.label(RichText::new("Max").color(COL_MUTED));
            ui.end_row();
            for (name, mm, unit, color) in [("U", a.v, "V", COL_V), ("I", a.i, "A", COL_I), ("P", a.p, "W", COL_P)] {
                ui.label(RichText::new(name).color(color).strong());
                if mm.n == 0 {
                    ui.label("–");
                    ui.label("–");
                    ui.label("–");
                } else {
                    ui.monospace(format::si(mm.min, unit, 4));
                    ui.monospace(format::si(mm.avg().unwrap_or(0.0), unit, 4));
                    ui.monospace(format::si(mm.max, unit, 4));
                }
                ui.end_row();
            }
            if dmm_stats.n > 0 {
                ui.label(RichText::new("DMM").color(COL_DMM).strong());
                ui.monospace(format::si(dmm_stats.min, dmm_unit, 4));
                ui.monospace(format::si(dmm_stats.avg().unwrap_or(0.0), dmm_unit, 4));
                ui.monospace(format::si(dmm_stats.max, dmm_unit, 4));
                ui.end_row();
            }
        });
        egui::Grid::new("energy").num_columns(2).show(ui, |ui| {
            ui.label("Energie");
            ui.monospace(format::si(a.energy_wh, "Wh", 4));
            ui.end_row();
            ui.label("Ladung");
            ui.monospace(format::si(a.charge_ah, "Ah", 4));
            ui.end_row();
            ui.label("Welligkeit (1 s)");
            ui.monospace(ripple.map_or("–".into(), |r| format::si(r, "Vpp", 3)));
            ui.end_row();
            ui.label("Abtastrate");
            ui.monospace(format!("{power_rate} Hz / DMM {dmm_rate} Hz"));
            ui.end_row();
        });
    }

    fn dmm_controls(&mut self, ui: &mut egui::Ui) {
        let current = self.store.lock().unwrap().dmm.function;
        let spm = self.settings.dmm_kind == DmmKind::OwonSpm;
        let functions: &[DmmFunction] = if spm { &DmmFunction::SPM_SELECTABLE } else { &DmmFunction::SELECTABLE };
        ui.horizontal_wrapped(|ui| {
            for &f in functions {
                if ui.selectable_label(current == f, f.label()).clicked()
                    && let Some(d) = self.dmm_handle()
                {
                    d.send(DmmCommand::SetFunction(f));
                }
            }
        });
        if spm {
            // The SPM's meter has no rate setting.
            return;
        }
        ui.horizontal(|ui| {
            ui.label("Messrate");
            for r in [DmmRate::Fast, DmmRate::Medium, DmmRate::Slow] {
                if ui.selectable_value(&mut self.settings.dmm_rate, r, r.label()).clicked()
                    && let Some(d) = &self.dmm_dev
                {
                    d.send(DmmCommand::SetRate(r));
                }
            }
        });
    }

    fn events(&mut self, ui: &mut egui::Ui) {
        let (events, now) = {
            let s = self.store.lock().unwrap();
            (s.power.analyzer.events.iter().rev().take(200).cloned().collect::<Vec<_>>(), s.now())
        };
        if events.is_empty() {
            ui.label(RichText::new("Noch nichts passiert.").color(COL_MUTED));
            return;
        }
        egui::Grid::new("events").num_columns(3).striped(true).show(ui, |ui| {
            for e in events {
                let color = match e.kind {
                    EventKind::Dropout | EventKind::Short => COL_CC,
                    EventKind::CurrentLimit => COL_P,
                    EventKind::Protection => COL_PROT,
                    EventKind::Marker => Color32::WHITE,
                };
                let resp =
                    ui.add(egui::Label::new(RichText::new(clock(e.t_start)).monospace()).sense(egui::Sense::click()));
                if resp.on_hover_text("Im Graph anzeigen").clicked() {
                    let span = (e.duration(now) * 4.0).max(1.0);
                    self.paused = true;
                    self.view_x = (e.t_start - span, e.t_start + span);
                }
                ui.label(RichText::new(e.kind.label()).color(color));
                if e.kind == EventKind::Marker {
                    ui.label("");
                } else if !e.v_min.is_finite() {
                    ui.monospace(format::duration(e.duration(now)));
                } else {
                    ui.monospace(format!(
                        "{} · {:.2} V · {:.3} A",
                        format::duration(e.duration(now)),
                        e.v_min,
                        e.i_max
                    ));
                }
                ui.end_row();
            }
        });
    }

    /// Calibration of the in-line meter against the multimeter. Returns
    /// true when the calibration changed.
    fn calibration_ui(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;
        ui.label(RichText::new("Kalibrierung Messmodul (nur PowerMon-Box)").strong());
        ui.label(
            RichText::new(
                "Gilt nur für die Messwerte der PowerMon-Box. Das OWON SPM misst mit seiner eigenen Kalibrierung.",
            )
            .small()
            .color(COL_MUTED),
        );
        let cal = &mut self.settings.calibration;
        ui.horizontal(|ui| {
            ui.label("U ×");
            changed |=
                ui.add(egui::DragValue::new(&mut cal.v_gain).speed(0.0001).range(0.5..=2.0).max_decimals(5)).changed();
            ui.label("I ×");
            changed |=
                ui.add(egui::DragValue::new(&mut cal.i_gain).speed(0.0001).range(0.5..=2.0).max_decimals(5)).changed();
            ui.label("I-Offset");
            changed |= ui
                .add(
                    egui::DragValue::new(&mut cal.i_offset)
                        .speed(0.00001)
                        .range(-0.5..=0.5)
                        .max_decimals(6)
                        .suffix(" A"),
                )
                .changed();
            if ui.small_button("Zurücksetzen").clicked() {
                *cal = Calibration::default();
                changed = true;
            }
        });
        let (power, dmm, func, conn) = {
            let s = self.store.lock().unwrap();
            (s.avg_power(1.0), s.avg_dmm(1.0), s.dmm.function, s.power.conn.is_connected())
        };
        // Only samples from the box go through this calibration; adjusting
        // it from SPM samples would change nothing on screen and add up.
        let from_box = conn
            && match self.settings.power_kind {
                PowerSourceKind::PowerMon => true,
                PowerSourceKind::OwonSpm => self.settings.spm_use_box && self.box_dev.is_some(),
                PowerSourceKind::SpmSimulator | PowerSourceKind::Simulator => false,
            };
        let power = power.filter(|_| from_box);
        let not_box = "Nur mit Messwerten der PowerMon-Box";
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(power.is_some(), egui::Button::new("Strom-Nullpunkt"))
                .on_hover_text("Ausgang ohne Last: der angezeigte Reststrom wird zum neuen Nullpunkt.")
                .on_disabled_hover_text(not_box)
                .clicked()
                && let Some((_, i)) = power
            {
                cal.i_offset += i / cal.i_gain;
                changed = true;
            }
            let v_ok = func == DmmFunction::VoltDc && power.is_some_and(|p| p.0 > 0.5) && dmm.is_some();
            if ui
                .add_enabled(v_ok, egui::Button::new("U an Multimeter angleichen"))
                .on_hover_text("Multimeter (V DC) parallel an den Ausgang klemmen, dann klicken.")
                .on_disabled_hover_text(if from_box { "Multimeter auf V DC, Ausgang über 0,5 V" } else { not_box })
                .clicked()
                && let (Some((v, _)), Some(d)) = (power, dmm)
            {
                cal.v_gain *= d / v;
                changed = true;
            }
            let i_ok = func == DmmFunction::CurrDc && power.is_some_and(|p| p.1 > 0.01) && dmm.is_some();
            if ui
                .add_enabled(i_ok, egui::Button::new("I an Multimeter angleichen"))
                .on_hover_text("Multimeter (A DC) in Reihe zur Last schalten, dann klicken.")
                .on_disabled_hover_text(if from_box { "Multimeter auf A DC, Strom über 10 mA" } else { not_box })
                .clicked()
                && let (Some((_, i)), Some(d)) = (power, dmm)
            {
                cal.i_gain *= d / i;
                changed = true;
            }
        });
        changed
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        let mut restart_overlay = false;
        let mut changed = false;
        let mut reconnect_spm = false;
        egui::Window::new("Einstellungen").open(&mut open).resizable(false).show(ctx, |ui| {
            egui::Grid::new("settings").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("Anzeige glätten");
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut self.settings.display_avg_ms)
                            .range(0.0..=1000.0)
                            .speed(5.0)
                            .suffix(" ms"),
                    )
                    .on_hover_text(
                        "Mittelwert für die großen Zahlen. 0 = jeder Messwert roh. Graphen zeigen immer Rohdaten.",
                    )
                    .changed();
                ui.end_row();
                ui.label("Einbruch ab");
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut self.settings.analysis.dropout_pct)
                            .range(0.5..=50.0)
                            .speed(0.1)
                            .suffix(" % unter Soll"),
                    )
                    .changed();
                ui.end_row();
                ui.label("CV-Toleranz");
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut self.settings.analysis.cv_tol_pct)
                            .range(0.1..=20.0)
                            .speed(0.1)
                            .suffix(" %"),
                    )
                    .changed();
                ui.end_row();
                ui.label("CC-Toleranz");
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut self.settings.analysis.cc_tol_pct)
                            .range(0.1..=20.0)
                            .speed(0.1)
                            .suffix(" %"),
                    )
                    .changed();
                ui.end_row();
                ui.label("Kurzschluss unter");
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut self.settings.analysis.short_volts)
                            .range(0.01..=5.0)
                            .speed(0.01)
                            .suffix(" V"),
                    )
                    .changed();
                ui.end_row();
                ui.label("DMM-Stellen");
                restart_overlay |= ui.add(egui::DragValue::new(&mut self.settings.dmm_digits).range(3..=7)).changed();
                ui.end_row();
                ui.label("Baudrate Messmodul");
                baud_combo(ui, "pm_baud", &mut self.settings.power_baud);
                ui.end_row();
                ui.label("Baudrate Multimeter");
                baud_combo(ui, "dmm_baud", &mut self.settings.dmm_baud);
                ui.end_row();
                ui.label("Overlay-Port");
                restart_overlay |=
                    ui.add(egui::DragValue::new(&mut self.settings.overlay_port).range(1024..=65535)).changed();
                ui.end_row();
                ui.label("");
                restart_overlay |= ui
                    .checkbox(&mut self.settings.overlay_lan, "Overlay im Netzwerk freigeben")
                    .on_hover_text("Für OBS auf einem anderen Rechner. Sonst nur lokal erreichbar.")
                    .changed();
                ui.end_row();
                ui.label("");
                if ui.checkbox(&mut self.settings.always_on_top, "Fenster immer im Vordergrund").changed() {
                    let level = if self.settings.always_on_top {
                        egui::WindowLevel::AlwaysOnTop
                    } else {
                        egui::WindowLevel::Normal
                    };
                    ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
                }
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut self.settings.auto_connect, "Beim Start automatisch verbinden");
                ui.end_row();
            });
            ui.separator();
            reconnect_spm = self.spm_settings_ui(ui);
            ui.separator();
            changed |= self.calibration_ui(ui);
            ui.separator();
            ui.label(
                RichText::new(
                    "Tasten: Leertaste = Pause · M = Marker · R = Statistik zurücksetzen · E = CSV-Export · \
                     O = Ausgang AUS (OWON SPM)",
                )
                .small(),
            );
        });
        self.show_settings = open;
        if reconnect_spm && self.settings.power_kind.is_spm() && self.power_running() {
            self.connect_power(ctx);
        }
        if changed {
            self.sync_analysis();
        }
        if restart_overlay {
            self.restart_overlay();
        }
    }

    /// OWON SPM options. Returns true when the SPM must reconnect.
    fn spm_settings_ui(&mut self, ui: &mut egui::Ui) -> bool {
        let before = (
            self.settings.spm_baud,
            self.settings.spm_use_box,
            self.settings.spm_box_port.clone(),
            self.settings.spm_lock_panel,
        );
        ui.label(RichText::new("OWON SPM").strong());
        egui::Grid::new("spm_settings").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Baudrate");
            baud_combo(ui, "spm_baud", &mut self.settings.spm_baud);
            ui.end_row();
            ui.label("");
            ui.checkbox(&mut self.settings.spm_use_box, "Messwerte von der PowerMon-Box (100 Hz)").on_hover_text(
                "Das SPM liefert Sollwerte, CV/CC, Steuerung und Multimeter, die Box die schnellen Messwerte.",
            );
            ui.end_row();
            if self.settings.spm_use_box {
                if self.settings.spm_box_port.is_empty() {
                    self.settings.spm_box_port = self.settings.power_port.clone();
                }
                ui.label("Port der Box");
                port_combo(ui, "spm_box_port", &mut self.settings.spm_box_port, &self.ports);
                ui.end_row();
            }
            ui.label("");
            ui.checkbox(&mut self.settings.spm_lock_panel, "Bedienfeld am Netzteil sperren")
                .on_hover_text("Sperrt die Tasten am Netzteil, solange die App verbunden ist.");
            ui.end_row();
            ui.label("Spannungsgrenze");
            ui.horizontal(|ui| {
                let mut on = self.settings.psu_v_guard.is_some();
                if ui
                    .checkbox(&mut on, "")
                    .on_hover_text("Die App stellt nie mehr als diese Spannung ein und schaltet darüber nicht ein.")
                    .changed()
                {
                    self.settings.psu_v_guard = on.then_some(20.0);
                }
                if let Some(g) = &mut self.settings.psu_v_guard {
                    ui.add(
                        egui::DragValue::new(g)
                            .range(0.0..=60.0)
                            .speed(0.1)
                            .suffix(" V")
                            .max_decimals(2)
                            .custom_parser(parse_decimal),
                    );
                } else {
                    ui.label(RichText::new("aus").color(COL_MUTED));
                }
            });
            ui.end_row();
        });
        before
            != (
                self.settings.spm_baud,
                self.settings.spm_use_box,
                self.settings.spm_box_port.clone(),
                self.settings.spm_lock_panel,
            )
    }

    fn hotkeys(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let (space, m, r, e, o) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Space),
                i.key_pressed(egui::Key::M),
                i.key_pressed(egui::Key::R),
                i.key_pressed(egui::Key::E),
                i.key_pressed(egui::Key::O),
            )
        });
        // Panic button: output off, right now. There is deliberately no key
        // that switches it on.
        if o && (self.psu_handle().is_some() || self.settings.power_kind.is_spm()) {
            let connected = self.store.lock().unwrap().power.psu_conn.is_connected();
            // Queued even while reconnecting: the driver delivers a pending
            // OFF as soon as the supply answers again.
            let sent = self.psu_handle().is_some_and(|h| h.send_psu(PsuCommand::Output(false)));
            if sent {
                self.output_cmd = Some((false, Instant::now()));
            }
            if sent && connected {
                self.toast("Ausgang AUS (Taste O)");
            } else {
                self.toast("Netzteil nicht verbunden – Ausgang NICHT geschaltet. Am Gerät ausschalten!");
            }
        }
        if space {
            self.paused = !self.paused;
        }
        if m {
            self.store.lock().unwrap().add_marker();
            self.toast("Marker gesetzt");
        }
        if r {
            self.store.lock().unwrap().power.analyzer.reset_stats();
            self.toast("Statistik zurückgesetzt");
        }
        if e {
            self.export();
        }
    }
}

impl eframe::App for PowerMeterApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.hotkeys(&ctx);
        self.flush_deferred_psu();

        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("bottom").show(ui, |ui| {
            ui.horizontal(|ui| {
                let (pc, psu, dc) = {
                    let s = self.store.lock().unwrap();
                    (s.power.conn.clone(), s.power.psu_conn.clone(), s.dmm.conn.clone())
                };
                if self.settings.power_kind.is_spm() {
                    ui.label(RichText::new(format!("Netzteil: {}", conn_text(&psu))).small().color(COL_MUTED));
                    if self.box_dev.is_some() {
                        ui.separator();
                        ui.label(RichText::new(format!("Box: {}", conn_text(&pc))).small().color(COL_MUTED));
                    }
                } else {
                    ui.label(RichText::new(format!("Netzteil: {}", conn_text(&pc))).small().color(COL_MUTED));
                }
                ui.separator();
                ui.label(RichText::new(format!("Multimeter: {}", conn_text(&dc))).small().color(COL_MUTED));
                if let Some((msg, at)) = &self.toast {
                    if at.elapsed() < Duration::from_secs(4) {
                        ui.separator();
                        ui.label(RichText::new(msg).small());
                    } else {
                        self.toast = None;
                    }
                }
            });
        });
        egui::Panel::right("side").default_size(340.0).show(ui, |ui| self.side_panel(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            self.readouts(ui);
            ui.add_space(8.0);
            self.plots(ui);
        });
        if self.show_settings {
            self.settings_window(&ctx);
        }
        // Keep the "stale" detection and toasts ticking even with no data.
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, "settings", &self.settings);
    }
}

/// (name, unit, color, points, set point)
type PlotRow<'a> = (&'a str, &'a str, Color32, &'a Vec<[f64; 2]>, Option<f64>);

// ------------------------------------------------------------------ widgets

/// What "Übernehmen" sends right away (in this order), which lowered
/// OVP/OCP (index into the edit, value) wait for the output to drop below
/// them, and what to tell the user.
#[derive(Debug, Default, PartialEq)]
struct EditPlan {
    now: Vec<PsuCommand>,
    deferred: Vec<(usize, f64)>,
    notes: Vec<String>,
}

/// Order matters, because the supply needs about a second to ramp: a raised
/// OVP/OCP goes out before the set points, a lowered one only once the
/// output is below it – otherwise the old or new threshold trips while the
/// output moves. `edit` is U, I, OVP, OCP as typed.
fn plan_psu_edit(edit: [Option<f64>; 4], psu: &PsuState, v_cap: f64) -> EditPlan {
    let mut plan = EditPlan::default();
    let limits = [v_cap, psu.i_max, ovp_limit(psu.v_max), ovp_limit(psu.i_max)];
    let device = [psu.v_set, psu.i_set, psu.ovp, psu.ocp];
    let mut send: [Option<f64>; 4] = [None; 4];
    for k in 0..4 {
        let Some(want) = edit[k].filter(|x| x.is_finite()) else { continue };
        let value = want.clamp(0.0, limits[k]);
        if k == 0 && value < want - 0.0005 {
            plan.notes.push(format!("Spannung auf {} V begrenzt", format::fixed(value, 2)));
        }
        if device[k].is_some_and(|d| (d - value).abs() < 0.0005) {
            continue; // already set
        }
        send[k] = Some(value);
    }
    // A set voltage at or above the OVP in force trips the supply.
    if let (Some(v), Some(ovp)) = (send[0], send[2].or(psu.ovp))
        && v >= ovp - 0.0005
    {
        plan.notes.push(format!(
            "Nicht gesendet: {} V liegt nicht unter OVP {} V – OVP mit erhöhen",
            format::fixed(v, 2),
            format::fixed(ovp, 2)
        ));
        send[0] = None;
    }
    // An OVP at or below the set voltage would trip right away.
    if let (Some(ovp), Some(v)) = (send[2], send[0].or(psu.v_set))
        && ovp <= v + 0.0005
    {
        plan.notes.push(format!(
            "Nicht gesendet: OVP {} V liegt nicht über der Sollspannung {} V",
            format::fixed(ovp, 2),
            format::fixed(v, 2)
        ));
        send[2] = None;
    }
    for k in [2, 3, 0, 1] {
        let Some(x) = send[k] else { continue };
        if k >= 2 && device[k].is_some_and(|d| x < d) {
            plan.deferred.push((k, x));
        } else {
            plan.now.push(psu_command(k, x));
        }
    }
    plan
}

fn psu_command(k: usize, x: f64) -> PsuCommand {
    match k {
        0 => PsuCommand::SetVoltage(x),
        1 => PsuCommand::SetCurrent(x),
        2 => PsuCommand::SetOvp(x),
        _ => PsuCommand::SetOcp(x),
    }
}

/// Number typed into a field: decimal comma or point, unit optional
/// ("0,5", "12.5 V").
fn parse_decimal(s: &str) -> Option<f64> {
    let t = s.trim().trim_end_matches(|c: char| c.is_alphabetic() || c.is_whitespace());
    t.replace(',', ".").parse().ok()
}

fn clock(t: f64) -> String {
    let neg = t < 0.0;
    let t = t.abs();
    let m = (t / 60.0).floor();
    let s = t - m * 60.0;
    format!("{}{}:{:04.1}", if neg { "-" } else { "" }, m as u64, s)
}

fn conn_text(c: &ConnState) -> String {
    match c {
        ConnState::Disconnected => "getrennt".into(),
        ConnState::Connecting => "verbinde …".into(),
        ConnState::Connected(d) => format!("verbunden ({d})"),
        ConnState::Error(e) => format!("Fehler: {e}"),
    }
}

fn conn_dot(ui: &mut egui::Ui, c: &ConnState) {
    let color = match c {
        ConnState::Connected(_) => COL_CV,
        ConnState::Connecting => COL_P,
        ConnState::Error(_) => COL_CC,
        ConnState::Disconnected => COL_MUTED,
    };
    ui.label(RichText::new("●").color(color)).on_hover_text(conn_text(c));
}

fn port_combo(ui: &mut egui::Ui, id: &str, port: &mut String, ports: &[String]) {
    let shown =
        if port.is_empty() { "Port wählen …".to_string() } else { port.trim_start_matches("/dev/").to_string() };
    egui::ComboBox::from_id_salt(id).selected_text(shown).width(170.0).show_ui(ui, |ui| {
        if ports.is_empty() {
            ui.label("Kein USB-Gerät gefunden");
        }
        for p in ports {
            ui.selectable_value(port, p.clone(), p.trim_start_matches("/dev/"));
        }
    });
}

fn baud_combo(ui: &mut egui::Ui, id: &str, baud: &mut u32) {
    egui::ComboBox::from_id_salt(id).selected_text(baud.to_string()).show_ui(ui, |ui| {
        for b in [9_600, 19_200, 38_400, 57_600, 115_200, 230_400, 460_800, 921_600] {
            ui.selectable_value(baud, b, b.to_string());
        }
    });
}

/// "Auto" checkbox plus a value editor. Returns true when changed.
fn optional_value(
    ui: &mut egui::Ui,
    value: &mut Option<f64>,
    default: f64,
    suffix: &str,
    speed: f64,
    learned: Option<f64>,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let mut manual = value.is_some();
        if ui.checkbox(&mut manual, "").on_hover_text("Haken = fester Wert, sonst automatisch").changed() {
            *value = manual.then(|| learned.unwrap_or(default));
            changed = true;
        }
        match value {
            Some(v) => {
                changed |= ui
                    .add(egui::DragValue::new(v).speed(speed).range(0.0..=1000.0).suffix(suffix).max_decimals(3))
                    .changed();
            }
            None => {
                let text = learned.map_or("auto (noch unbekannt)".to_string(), |l| format!("auto: {l:.3}{suffix}"));
                ui.label(RichText::new(text).color(COL_MUTED));
            }
        }
    });
    changed
}

fn badge(ui: &mut egui::Ui, text: &str, fill: Color32) {
    egui::Frame::new().fill(fill).corner_radius(8.0).inner_margin(egui::Margin::symmetric(8, 1)).show(ui, |ui| {
        ui.label(RichText::new(text).monospace().strong().color(Color32::BLACK));
    });
}

#[allow(clippy::too_many_arguments)]
fn readout(
    ui: &mut egui::Ui,
    width: f32,
    size: f32,
    label: &str,
    value: String,
    unit: &str,
    color: Color32,
    sub: Option<String>,
    badge: Option<(&str, Color32)>,
) {
    egui::Frame::new()
        .fill(Color32::from_rgb(0x1c, 0x1e, 0x24))
        .corner_radius(12.0)
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.set_width(width - 28.0);
                ui.set_max_width(width - 28.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(label).small().color(COL_MUTED).strong());
                    if let Some((text, c)) = badge {
                        self::badge(ui, text, c);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(RichText::new(value).monospace().size(size).color(color).strong());
                    ui.label(RichText::new(unit).size(size * 0.45).color(COL_MUTED));
                });
                ui.label(RichText::new(sub.unwrap_or_default()).small().color(COL_MUTED));
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn psu(v: f64, i: f64, ovp: f64, ocp: f64) -> PsuState {
        PsuState {
            v_set: Some(v),
            i_set: Some(i),
            ovp: Some(ovp),
            ocp: Some(ocp),
            output_on: Some(true),
            v_max: 60.0,
            i_max: 10.0,
            ..Default::default()
        }
    }

    #[test]
    fn raised_thresholds_go_out_before_the_set_points() {
        let p = plan_psu_edit([Some(12.0), Some(2.0), Some(13.2), Some(2.5)], &psu(5.0, 1.0, 5.5, 1.5), 60.0);
        use PsuCommand::*;
        assert_eq!(p.now, vec![SetOvp(13.2), SetOcp(2.5), SetVoltage(12.0), SetCurrent(2.0)]);
        assert!(p.deferred.is_empty() && p.notes.is_empty());
    }

    #[test]
    fn lowered_thresholds_wait_for_the_output() {
        let p = plan_psu_edit([Some(5.0), None, Some(5.5), None], &psu(12.0, 1.0, 13.0, 1.5), 60.0);
        assert_eq!(p.now, vec![PsuCommand::SetVoltage(5.0)]);
        assert_eq!(p.deferred, vec![(2, 5.5)]);
    }

    #[test]
    fn voltage_above_ovp_is_refused() {
        let p = plan_psu_edit([Some(12.0), None, None, None], &psu(5.0, 1.0, 5.5, 1.5), 60.0);
        assert!(p.now.is_empty());
        assert!(p.notes[0].contains("OVP"), "{:?}", p.notes);
        // an OVP below the set voltage as well
        let p = plan_psu_edit([None, None, Some(4.0), None], &psu(5.0, 1.0, 5.5, 1.5), 60.0);
        assert!(p.now.is_empty() && p.deferred.is_empty());
    }

    #[test]
    fn guard_clamps_with_a_note() {
        let p = plan_psu_edit([Some(19.0), None, Some(25.0), None], &psu(5.0, 1.0, 5.5, 1.5), 12.0);
        assert_eq!(p.now, vec![PsuCommand::SetOvp(25.0), PsuCommand::SetVoltage(12.0)]);
        assert!(p.notes[0].contains("begrenzt"), "{:?}", p.notes);
    }

    #[test]
    fn fields_take_a_decimal_comma() {
        assert_eq!(parse_decimal("0,5"), Some(0.5));
        assert_eq!(parse_decimal("12,5 V"), Some(12.5));
        assert_eq!(parse_decimal(" 5.5V "), Some(5.5));
        assert_eq!(parse_decimal("0,"), Some(0.0));
        assert_eq!(parse_decimal("abc"), None);
    }
}
