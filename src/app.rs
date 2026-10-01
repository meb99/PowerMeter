//! The main window: connection bar, big readouts, live graphs, event list.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, Vec2b};
use egui_plot::{GridMark, Line, Plot, PlotPoints, Span, VLine};
use serde::{Deserialize, Serialize};

use crate::devices::{self, DeviceHandle, DmmCommand, DmmKind, DmmRate, PowerSourceKind};
use crate::format;
use crate::model::{
    AnalysisSettings, Calibration, ConnState, DmmFunction, EventKind, PowerSample, RegMode, Shared, Store, decimate,
    first_index_after,
};
use crate::overlay::OverlayServer;

const COL_V: Color32 = Color32::from_rgb(0xff, 0xd6, 0x0a);
const COL_I: Color32 = Color32::from_rgb(0x4c, 0xc9, 0xf0);
const COL_P: Color32 = Color32::from_rgb(0xff, 0x9f, 0x0a);
const COL_DMM: Color32 = Color32::from_rgb(0xb3, 0x88, 0xff);
const COL_CV: Color32 = Color32::from_rgb(0x30, 0xd1, 0x58);
const COL_CC: Color32 = Color32::from_rgb(0xff, 0x45, 0x3a);
const COL_MUTED: Color32 = Color32::from_rgb(0x8e, 0x8e, 0x93);

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
    dmm_dev: Option<DeviceHandle>,
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
            dmm_dev: None,
            overlay: None,
            ports: devices::list_ports(),
            last_port_scan: Instant::now(),
            paused: false,
            view_x: (0.0, 30.0),
            toast: None,
            show_settings: false,
        };
        app.restart_overlay();
        if app.settings.auto_connect {
            if app.settings.power_kind == PowerSourceKind::Simulator || !app.settings.power_port.is_empty() {
                app.connect_power(&cc.egui_ctx);
            }
            if app.settings.dmm_kind == DmmKind::Simulator || !app.settings.dmm_port.is_empty() {
                app.connect_dmm(&cc.egui_ctx);
            }
        }
        app
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

    fn connect_power(&mut self, ctx: &egui::Context) {
        self.power_dev = None;
        let store = self.store.clone();
        let ctx = ctx.clone();
        self.power_dev = Some(match self.settings.power_kind {
            PowerSourceKind::Simulator => {
                DeviceHandle::spawn("power-sim", move |stop| devices::sim::run_power(store, ctx, 100.0, stop))
            }
            PowerSourceKind::PowerMon => {
                let cfg = devices::powermon::PowerMonConfig {
                    port: self.settings.power_port.clone(),
                    baud: self.settings.power_baud,
                };
                DeviceHandle::spawn("powermon", move |stop| devices::powermon::run(cfg, store, ctx, stop))
            }
        });
    }

    fn connect_dmm(&mut self, ctx: &egui::Context) {
        self.dmm_dev = None;
        let store = self.store.clone();
        let ctx = ctx.clone();
        let (tx, rx) = mpsc::channel();
        let mut handle = match self.settings.dmm_kind {
            DmmKind::Simulator => {
                DeviceHandle::spawn("dmm-sim", move |stop| devices::sim::run_dmm(store, ctx, rx, stop))
            }
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
            let conn = self.store.lock().unwrap().power.conn.clone();
            ui.label(RichText::new("Netzteil").strong());
            let before = self.settings.power_kind;
            egui::ComboBox::from_id_salt("power_kind").selected_text(self.settings.power_kind.label()).show_ui(
                ui,
                |ui| {
                    for k in [PowerSourceKind::PowerMon, PowerSourceKind::Simulator] {
                        ui.selectable_value(&mut self.settings.power_kind, k, k.label());
                    }
                },
            );
            if self.settings.power_kind == PowerSourceKind::PowerMon {
                port_combo(ui, "power_port", &mut self.settings.power_port, &self.ports);
            }
            let changed = before != self.settings.power_kind;
            if self.power_dev.as_ref().is_some_and(|d| !d.is_finished()) && !changed {
                if ui.button("Trennen").clicked() {
                    self.power_dev = None;
                }
            } else if ui.button("Verbinden").clicked() || (changed && self.power_dev.is_some()) {
                self.connect_power(&ctx);
            }
            conn_dot(ui, &conn);

            ui.separator();

            // multimeter
            let conn = self.store.lock().unwrap().dmm.conn.clone();
            ui.label(RichText::new("Multimeter").strong());
            let before = self.settings.dmm_kind;
            egui::ComboBox::from_id_salt("dmm_kind").selected_text(self.settings.dmm_kind.label()).show_ui(ui, |ui| {
                for k in [DmmKind::OwonXdm, DmmKind::Simulator] {
                    ui.selectable_value(&mut self.settings.dmm_kind, k, k.label());
                }
            });
            if self.settings.dmm_kind == DmmKind::OwonXdm {
                port_combo(ui, "dmm_port", &mut self.settings.dmm_port, &self.ports);
            }
            let changed = before != self.settings.dmm_kind;
            if self.dmm_dev.as_ref().is_some_and(|d| !d.is_finished()) && !changed {
                if ui.button("Trennen").clicked() {
                    self.dmm_dev = None;
                }
            } else if ui.button("Verbinden").clicked() || (changed && self.dmm_dev.is_some()) {
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
        let (disp, mode, v_set, i_set, dmm, dmm_fn, dmm_conn, power_conn, short, dip) = {
            let s = self.store.lock().unwrap();
            let a = &s.power.analyzer;
            let now = s.now();
            let disp = s.display_power().filter(|p| now - p.t < 2.0);
            let dmm = s.dmm.samples.back().filter(|d| now - d.t < 3.0).map(|d| d.value);
            (
                disp,
                a.mode,
                a.v_set_effective(&s.analysis),
                a.i_limit(&s.analysis),
                dmm,
                s.dmm.function,
                s.dmm.conn.is_connected(),
                s.power.conn.is_connected(),
                a.active(EventKind::Short),
                a.active(EventKind::Dropout),
            )
        };
        let avail = ui.available_width();
        let cols = if dmm_conn { 4.0 } else { 3.0 };
        let w = ((avail - (cols - 1.0) * 8.0) / cols).max(150.0);
        let size = ((w - 28.0) / 5.4).clamp(24.0, 72.0);

        ui.horizontal(|ui| {
            let sub_v = v_set.map(|v| format!("Soll {} V", format::fixed(v, 2)));
            let sub_i = i_set.map(|i| format!("Limit {} A", format::fixed(i, 3)));
            let alarm = if short {
                Some(("KURZSCHLUSS", COL_CC))
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
                            _ => (Color32::from_rgba_unmultiplied(255, 159, 10, 30), "CC"),
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
            ui.label(
                RichText::new(
                    "Das Netzteil hat keine Schnittstelle – Sollwerte hier eintragen oder automatisch lernen lassen.",
                )
                .small()
                .color(COL_MUTED),
            );
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
        ui.horizontal_wrapped(|ui| {
            for f in DmmFunction::SELECTABLE {
                if ui.selectable_label(current == f, f.label()).clicked()
                    && let Some(d) = &self.dmm_dev
                {
                    d.send(DmmCommand::SetFunction(f));
                }
            }
        });
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
        ui.label(RichText::new("Kalibrierung Messmodul").strong());
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
        let (power, dmm, func) = {
            let s = self.store.lock().unwrap();
            (s.avg_power(1.0), s.avg_dmm(1.0), s.dmm.function)
        };
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(power.is_some(), egui::Button::new("Strom-Nullpunkt"))
                .on_hover_text("Ausgang ohne Last: der angezeigte Reststrom wird zum neuen Nullpunkt.")
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
            changed |= self.calibration_ui(ui);
            ui.separator();
            ui.label(
                RichText::new("Tasten: Leertaste = Pause · M = Marker · R = Statistik zurücksetzen · E = CSV-Export")
                    .small(),
            );
        });
        self.show_settings = open;
        if changed {
            self.sync_analysis();
        }
        if restart_overlay {
            self.restart_overlay();
        }
    }

    fn hotkeys(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let (space, m, r, e) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Space),
                i.key_pressed(egui::Key::M),
                i.key_pressed(egui::Key::R),
                i.key_pressed(egui::Key::E),
            )
        });
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

        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("bottom").show(ui, |ui| {
            ui.horizontal(|ui| {
                let (pc, dc) = {
                    let s = self.store.lock().unwrap();
                    (s.power.conn.clone(), s.dmm.conn.clone())
                };
                ui.label(RichText::new(format!("Netzteil: {}", conn_text(&pc))).small().color(COL_MUTED));
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
                        egui::Frame::new().fill(c).corner_radius(8.0).inner_margin(egui::Margin::symmetric(8, 1)).show(
                            ui,
                            |ui| {
                                ui.label(RichText::new(text).monospace().strong().color(Color32::BLACK));
                            },
                        );
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
