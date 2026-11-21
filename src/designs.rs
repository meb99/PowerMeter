//! Overlay designs: how the OBS overlay looks, made in the Overlay-Studio
//! (`/studio`) and kept in `designs.json` in the app data folder.
//!
//! A [`DesignConfig`] is read leniently: unknown fields are ignored, a field
//! of the wrong type keeps its default, and [`DesignConfig::sanitize`]
//! clamps everything to sane ranges. Old, partial or hand-edited files never
//! fail to load. Templates live here and are served to the studio, so Rust is
//! the one place that knows what a template looks like.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

pub type SharedDesigns = Arc<Mutex<DesignStore>>;

pub const MAX_DESIGNS: usize = 50;
pub const MAX_CUSTOM_CSS: usize = 20 * 1024;
pub const MAX_ASSET_BYTES: usize = 5 * 1024 * 1024;
const MAX_TITLE: usize = 60;
const MAX_NAME: usize = 60;
const MAX_ID: usize = 40;
const FILE_NAME: &str = "designs.json";
const ASSET_DIR: &str = "overlay-assets";

/// Everything an overlay can show, in the default order.
pub const ELEMENT_KEYS: [&str; 13] =
    ["logo", "title", "mode", "model", "alarms", "set", "v", "i", "p", "graph", "energy", "events", "dmm"];

const SERIES_KEYS: [&str; 3] = ["v", "i", "p"];

// ------------------------------------------------------------------ fonts

/// A font choice of the studio. Only fonts that come with macOS or Windows
/// (OBS may run offline), each with fallbacks for the other system.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Font {
    pub id: &'static str,
    /// Named by its look, not by one system's font name.
    pub name: &'static str,
    pub css: &'static str,
    /// `font-stretch` to go with it (Bahnschrift's narrow cut).
    pub stretch: &'static str,
    /// Only for numbers (the built-in 7-segment digits).
    pub numbers_only: bool,
}

const fn font(id: &'static str, name: &'static str, css: &'static str) -> Font {
    Font { id, name, css, stretch: "normal", numbers_only: false }
}

pub const FONTS: [Font; 16] = [
    font(
        "b612",
        "B612 Mono (Messgerät)",
        "\"B612 Mono\", \"SF Mono\", \"Cascadia Mono\", Consolas, Menlo, \"DejaVu Sans Mono\", monospace",
    ),
    font(
        "system",
        "System (Standard)",
        "-apple-system, BlinkMacSystemFont, \"Segoe UI Variable Text\", \"Segoe UI\", system-ui, \"Helvetica Neue\", Arial, sans-serif",
    ),
    font(
        "mono",
        "Mono (Code)",
        "\"SF Mono\", ui-monospace, \"Cascadia Mono\", \"Cascadia Code\", Consolas, Menlo, \"DejaVu Sans Mono\", monospace",
    ),
    font(
        "mono-classic",
        "Mono klassisch",
        "Menlo, Monaco, Consolas, \"Lucida Console\", \"DejaVu Sans Mono\", monospace",
    ),
    font("typewriter", "Schreibmaschine", "\"Courier New\", Courier, \"Liberation Mono\", monospace"),
    font(
        "din",
        "DIN / Technisch",
        "\"DIN Alternate\", Bahnschrift, \"DIN 2014\", \"D-DIN\", \"Roboto Condensed\", \"Arial Narrow\", sans-serif",
    ),
    Font {
        stretch: "condensed",
        ..font(
            "din-narrow",
            "DIN schmal",
            "\"DIN Condensed\", \"Bahnschrift Condensed\", Bahnschrift, \"Arial Narrow\", \"Liberation Sans Narrow\", sans-serif",
        )
    },
    font("clear", "Klar (Helvetica / Arial)", "\"Helvetica Neue\", Helvetica, Arial, \"Liberation Sans\", sans-serif"),
    font(
        "modern",
        "Modern",
        "\"Avenir Next\", Avenir, \"Segoe UI Variable Display\", \"Segoe UI\", Inter, \"Helvetica Neue\", Arial, sans-serif",
    ),
    font("geometric", "Geometrisch", "Futura, \"Century Gothic\", \"Trebuchet MS\", \"Avenir Next\", sans-serif"),
    font(
        "rounded",
        "Rund",
        "\"Arial Rounded MT Bold\", \"Arial Rounded MT\", \"Nunito\", \"Varela Round\", \"Segoe UI\", Arial, sans-serif",
    ),
    font("bold", "Kräftig", "Impact, Haettenschweiler, \"Arial Narrow Bold\", \"Arial Black\", sans-serif"),
    font("elegant", "Elegant", "\"Gill Sans\", \"Gill Sans MT\", Calibri, \"Trebuchet MS\", sans-serif"),
    font("friendly", "Freundlich", "\"Trebuchet MS\", \"Lucida Grande\", \"Segoe UI\", Verdana, sans-serif"),
    font("wide", "Breit", "Verdana, Geneva, \"DejaVu Sans\", sans-serif"),
    Font {
        numbers_only: true,
        ..font(
            "seg7",
            "7-Segment (wie am Netzteil)",
            "\"SF Mono\", \"Cascadia Mono\", Consolas, Menlo, \"DejaVu Sans Mono\", monospace",
        )
    },
];

fn font_ok(id: &str, for_numbers: bool) -> bool {
    FONTS.iter().any(|f| f.id == id && (for_numbers || !f.numbers_only))
}

// ----------------------------------------------------------------- config

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    /// Card with the values stacked, today's look.
    #[default]
    Card,
    /// Instrument display: V | A | W side by side, unit below, status row.
    Meter,
    /// Everything in one line, for the bottom of the picture.
    Bar,
    /// No panel: big numbers with an outline, straight over the video.
    Minimal,
    /// Narrow vertical stack for the edge of the picture.
    Column,
    /// Front panel of a lab supply with glowing digits.
    Bench,
}

/// Which value is shown biggest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Accent {
    #[default]
    V,
    I,
    P,
    None,
}

/// Decimals of a value: `auto` follows the resolution of the source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Decimals {
    #[default]
    Auto,
    Fixed(u8),
}

impl Serialize for Decimals {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Decimals::Auto => s.serialize_str("auto"),
            Decimals::Fixed(n) => s.serialize_u8(*n),
        }
    }
}

impl<'de> Deserialize<'de> for Decimals {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        let n = match &v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        };
        Ok(match n {
            Some(n) if n.is_finite() => Decimals::Fixed(n.round().clamp(0.0, 4.0) as u8),
            _ => Decimals::Auto,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecimalsCfg {
    pub v: Decimals,
    pub i: Decimals,
    pub p: Decimals,
}

/// `#rrggbb` everywhere.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Colors {
    pub v: String,
    pub i: String,
    pub p: String,
    pub dmm: String,
    pub cv: String,
    pub cc: String,
    /// Warnings: short, dropout, protection.
    pub alarm: String,
    pub text: String,
    pub muted: String,
    pub bg: String,
    pub border: String,
}

impl Default for Colors {
    fn default() -> Self {
        let c = |s: &str| s.to_string();
        Self {
            v: c("#ffd60a"),
            i: c("#4cc9f0"),
            p: c("#ff9f0a"),
            dmm: c("#b388ff"),
            cv: c("#30d158"),
            cc: c("#ff453a"),
            alarm: c("#ff453a"),
            text: c("#f5f7fa"),
            muted: c("#9aa3ad"),
            bg: c("#0c0e12"),
            border: c("#ffffff"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphCfg {
    /// Subset of `v`, `i`, `p`.
    pub series: Vec<String>,
    pub seconds: f64,
    /// Smallest width; the graph fills a wider panel.
    pub width: f64,
    pub height: f64,
    pub line: f64,
    pub fill: bool,
    /// V scale on the left, A scale on the right.
    pub axes: bool,
    pub legend: bool,
    /// Dashed lines at the set voltage and the current limit.
    pub setpoints: bool,
}

impl Default for GraphCfg {
    fn default() -> Self {
        Self {
            series: vec!["v".into(), "i".into()],
            seconds: 20.0,
            width: 300.0,
            height: 70.0,
            line: 2.0,
            fill: false,
            axes: false,
            legend: false,
            setpoints: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Element {
    pub key: String,
    pub on: bool,
}

/// What one overlay design looks like. `Default` is the `klassisch`
/// template (the card as it always was).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DesignConfig {
    /// Template it was made from; "Farben aus Vorlage" resets to it.
    pub template: String,
    pub layout: Layout,
    /// Order and visibility of the elements, see [`ELEMENT_KEYS`].
    pub elements: Vec<Element>,
    pub title: String,
    /// Captions SPANNUNG / STROM / LEISTUNG.
    pub labels: bool,
    pub units: bool,
    pub accent: Accent,
    pub decimals: DecimalsCfg,
    /// Below 1 A / 1 W in mA / mW, with the decimals the source really has.
    pub auto_units: bool,
    pub colors: Colors,
    pub bg_opacity: f64,
    pub border_opacity: f64,
    pub border_width: f64,
    pub radius: f64,
    pub shadow: bool,
    /// Glowing text (and border), nice for `bench` and `neon`.
    pub glow: bool,
    pub font_text: String,
    pub font_numbers: String,
    pub weight: u32,
    pub scale: f64,
    pub graph: GraphCfg,
    /// Flash the border / banner on short, dropout and protection.
    pub alarm_flash: bool,
    pub hide_when_disconnected: bool,
    /// File name in the asset folder (see [`valid_asset_name`]).
    pub logo: Option<String>,
    pub logo_height: f64,
    /// Appended last, for power users.
    pub custom_css: String,
}

impl Default for DesignConfig {
    fn default() -> Self {
        let on = ["title", "mode", "alarms", "set", "v", "i", "p", "graph", "dmm"];
        Self {
            template: "klassisch".into(),
            layout: Layout::Card,
            elements: ELEMENT_KEYS.iter().map(|k| Element { key: (*k).into(), on: on.contains(k) }).collect(),
            title: "Netzteil".into(),
            labels: false,
            units: true,
            accent: Accent::V,
            decimals: DecimalsCfg::default(),
            auto_units: false,
            colors: Colors::default(),
            bg_opacity: 0.72,
            border_opacity: 0.08,
            border_width: 2.0,
            radius: 18.0,
            shadow: true,
            glow: false,
            font_text: "system".into(),
            font_numbers: "mono".into(),
            weight: 700,
            scale: 1.0,
            graph: GraphCfg::default(),
            alarm_flash: true,
            hide_when_disconnected: false,
            logo: None,
            logo_height: 40.0,
            custom_css: String::new(),
        }
    }
}

/// Takes `o[key]` into `slot` if it has the right shape; keeps the slot
/// otherwise.
fn take<T: DeserializeOwned>(o: &Map<String, Value>, key: &str, slot: &mut T) {
    if let Some(x) = o.get(key).and_then(|v| T::deserialize(v).ok()) {
        *slot = x;
    }
}

impl<'de> Deserialize<'de> for DesignConfig {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(DesignConfig::from_value(&Value::deserialize(d)?))
    }
}

impl DesignConfig {
    /// Reads whatever is usable from `v`, field by field, and sanitizes it.
    pub fn from_value(v: &Value) -> Self {
        let mut c = DesignConfig::default();
        let Some(o) = v.as_object() else { return c };
        take(o, "template", &mut c.template);
        take(o, "layout", &mut c.layout);
        if let Some(list) = o.get("elements").and_then(Value::as_array) {
            c.elements = list.iter().filter_map(|e| Element::deserialize(e).ok()).collect();
        }
        take(o, "title", &mut c.title);
        take(o, "labels", &mut c.labels);
        take(o, "units", &mut c.units);
        take(o, "accent", &mut c.accent);
        if let Some(d) = o.get("decimals").and_then(Value::as_object) {
            take(d, "v", &mut c.decimals.v);
            take(d, "i", &mut c.decimals.i);
            take(d, "p", &mut c.decimals.p);
        }
        // `milliamps` was the first name of the option
        take(o, "milliamps", &mut c.auto_units);
        take(o, "auto_units", &mut c.auto_units);
        if let Some(col) = o.get("colors").and_then(Value::as_object) {
            let k = &mut c.colors;
            for (key, slot) in [
                ("v", &mut k.v),
                ("i", &mut k.i),
                ("p", &mut k.p),
                ("dmm", &mut k.dmm),
                ("cv", &mut k.cv),
                ("cc", &mut k.cc),
                ("alarm", &mut k.alarm),
                ("text", &mut k.text),
                ("muted", &mut k.muted),
                ("bg", &mut k.bg),
                ("border", &mut k.border),
            ] {
                take(col, key, slot);
            }
        }
        take(o, "bg_opacity", &mut c.bg_opacity);
        take(o, "border_opacity", &mut c.border_opacity);
        take(o, "border_width", &mut c.border_width);
        take(o, "radius", &mut c.radius);
        take(o, "shadow", &mut c.shadow);
        take(o, "glow", &mut c.glow);
        take(o, "font_text", &mut c.font_text);
        take(o, "font_numbers", &mut c.font_numbers);
        if let Some(w) = o.get("weight").and_then(Value::as_f64) {
            c.weight = w.clamp(0.0, 10_000.0) as u32;
        }
        take(o, "scale", &mut c.scale);
        if let Some(g) = o.get("graph").and_then(Value::as_object) {
            take(g, "series", &mut c.graph.series);
            take(g, "seconds", &mut c.graph.seconds);
            take(g, "width", &mut c.graph.width);
            take(g, "height", &mut c.graph.height);
            take(g, "line", &mut c.graph.line);
            take(g, "fill", &mut c.graph.fill);
            take(g, "axes", &mut c.graph.axes);
            take(g, "legend", &mut c.graph.legend);
            take(g, "setpoints", &mut c.graph.setpoints);
        }
        take(o, "alarm_flash", &mut c.alarm_flash);
        take(o, "hide_when_disconnected", &mut c.hide_when_disconnected);
        take(o, "logo", &mut c.logo);
        take(o, "logo_height", &mut c.logo_height);
        take(o, "custom_css", &mut c.custom_css);
        c.sanitize();
        c
    }

    /// Clamps every field to what the renderer can show.
    pub fn sanitize(&mut self) {
        let def = DesignConfig::default();
        if template(&self.template).is_none() {
            self.template = def.template.clone();
        }
        // elements: known keys once each, in the given order, the rest off
        let mut seen = Vec::new();
        self.elements.retain(|e| {
            let ok = ELEMENT_KEYS.contains(&e.key.as_str()) && !seen.contains(&e.key);
            if ok {
                seen.push(e.key.clone());
            }
            ok
        });
        for k in ELEMENT_KEYS {
            if !seen.iter().any(|s| s == k) {
                self.elements.push(Element { key: k.into(), on: false });
            }
        }
        self.title = clean_text(&self.title, MAX_TITLE);
        let dc = Colors::default();
        let k = &mut self.colors;
        for (slot, fallback) in [
            (&mut k.v, dc.v),
            (&mut k.i, dc.i),
            (&mut k.p, dc.p),
            (&mut k.dmm, dc.dmm),
            (&mut k.cv, dc.cv),
            (&mut k.cc, dc.cc),
            (&mut k.alarm, dc.alarm),
            (&mut k.text, dc.text),
            (&mut k.muted, dc.muted),
            (&mut k.bg, dc.bg),
            (&mut k.border, dc.border),
        ] {
            *slot = clean_color(slot).unwrap_or(fallback);
        }
        self.bg_opacity = clamp(self.bg_opacity, 0.0, 1.0, def.bg_opacity);
        self.border_opacity = clamp(self.border_opacity, 0.0, 1.0, def.border_opacity);
        self.border_width = clamp(self.border_width, 0.0, 8.0, def.border_width);
        self.radius = clamp(self.radius, 0.0, 40.0, def.radius);
        if !font_ok(&self.font_text, false) {
            self.font_text = def.font_text.clone();
        }
        if !font_ok(&self.font_numbers, true) {
            self.font_numbers = def.font_numbers.clone();
        }
        self.weight = ((self.weight.clamp(400, 900) + 50) / 100 * 100).min(900);
        self.scale = clamp(self.scale, 0.4, 3.0, 1.0);
        let g = &mut self.graph;
        let mut series = Vec::new();
        for s in &g.series {
            if SERIES_KEYS.contains(&s.as_str()) && !series.contains(s) {
                series.push(s.clone());
            }
        }
        g.series = series;
        g.seconds = clamp(g.seconds, 5.0, 60.0, 20.0).round();
        g.width = clamp(g.width, 120.0, 1200.0, 300.0).round();
        g.height = clamp(g.height, 30.0, 240.0, 70.0).round();
        g.line = clamp(g.line, 1.0, 6.0, 2.0);
        self.logo = self.logo.take().filter(|f| valid_asset_name(f));
        self.logo_height = clamp(self.logo_height, 16.0, 200.0, def.logo_height).round();
        truncate_bytes(&mut self.custom_css, MAX_CUSTOM_CSS);
    }

    #[cfg(test)]
    pub fn is_on(&self, key: &str) -> bool {
        self.elements.iter().any(|e| e.key == key && e.on)
    }

    fn set_on(&mut self, key: &str, on: bool) {
        if let Some(e) = self.elements.iter_mut().find(|e| e.key == key) {
            e.on = on;
        }
    }

    /// The old `/overlay?show=…&layout=…` parameters on top of the
    /// `klassisch` template, so URLs already in OBS keep working.
    pub fn from_legacy_query(q: &HashMap<String, String>) -> Self {
        let mut c = DesignConfig::default();
        let list = |s: &str| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect::<Vec<_>>();
        if let Some(show) = q.get("show") {
            let show = list(show);
            for k in ["v", "i", "p", "mode", "set", "graph", "dmm", "energy", "events"] {
                c.set_on(k, show.iter().any(|s| s == k));
            }
            // The old header (title and warnings) came with the supply values.
            let power = ["v", "i", "p", "mode", "graph"].iter().any(|k| show.iter().any(|s| s == k));
            c.set_on("title", power);
            c.set_on("alarms", power);
        }
        if q.get("layout").map(String::as_str) == Some("bar") {
            c.layout = Layout::Bar;
            c.graph.height = 48.0;
            c.radius = 14.0;
            // The old bar showed all values the same size.
            c.accent = Accent::None;
        }
        if let Some(a) = q.get("accent") {
            c.accent = match a.as_str() {
                "i" => Accent::I,
                "p" => Accent::P,
                "none" => Accent::None,
                _ => Accent::V,
            };
        }
        if let Some(s) = q.get("scale").and_then(|s| s.parse::<f64>().ok()) {
            c.scale = s;
        }
        if let Some(b) = q.get("bg").and_then(|s| s.parse::<f64>().ok()) {
            c.bg_opacity = b;
        }
        if let Some(g) = q.get("graph") {
            c.graph.series = list(g);
        }
        c.sanitize();
        c
    }
}

/// True when the query has any of the old overlay parameters.
pub fn is_legacy_query(q: &HashMap<String, String>) -> bool {
    ["show", "layout", "scale", "bg", "graph", "accent"].iter().any(|k| q.contains_key(*k))
}

fn clamp(x: f64, lo: f64, hi: f64, fallback: f64) -> f64 {
    if x.is_finite() { x.clamp(lo, hi) } else { fallback }
}

/// Trimmed, one line, at most `max` characters.
fn clean_text(s: &str, max: usize) -> String {
    let one_line: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    one_line.trim().chars().take(max).collect::<String>().trim_end().to_string()
}

fn truncate_bytes(s: &mut String, max: usize) {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
}

/// `#rrggbb` in lower case, also from `#rgb`.
fn clean_color(s: &str) -> Option<String> {
    let h = s.trim().strip_prefix('#')?;
    if !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    match h.len() {
        6 => Some(format!("#{}", h.to_ascii_lowercase())),
        3 => Some(format!("#{}", h.chars().flat_map(|c| [c, c]).collect::<String>().to_ascii_lowercase())),
        _ => None,
    }
}

// -------------------------------------------------------------- templates

#[derive(Clone, Debug, Serialize)]
pub struct Template {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub cfg: DesignConfig,
}

fn elements(on: &[&str], order: &[&str]) -> Vec<Element> {
    let mut keys: Vec<&str> = order.to_vec();
    keys.extend(ELEMENT_KEYS.iter().filter(|k| !order.contains(k)));
    keys.into_iter().map(|k| Element { key: k.into(), on: on.contains(&k) }).collect()
}

/// All templates, in the order the studio shows them. The first one is
/// what a new install starts with.
pub fn templates() -> Vec<Template> {
    let base = DesignConfig::default();
    let s = |x: &str| x.to_string();

    // Instrument display: V | A | W in B612 Mono, a thin status row below.
    let messgeraet = DesignConfig {
        template: s("messgeraet"),
        layout: Layout::Meter,
        elements: elements(
            &["v", "i", "p", "mode", "model", "alarms", "set"],
            &["v", "i", "p", "mode", "model", "alarms", "set"],
        ),
        title: String::new(),
        accent: Accent::None,
        auto_units: true,
        colors: Colors {
            v: s("#00e5ff"),
            i: s("#ffb000"),
            p: s("#3ddc84"),
            dmm: s("#b388ff"),
            cv: s("#30d158"),
            cc: s("#ff9f0a"),
            alarm: s("#ff3b30"),
            text: s("#f2f4f7"),
            muted: s("#8b939e"),
            bg: s("#0b0d10"),
            border: s("#ffffff"),
        },
        bg_opacity: 0.86,
        border_opacity: 0.07,
        border_width: 1.0,
        radius: 16.0,
        font_text: s("system"),
        font_numbers: s("b612"),
        weight: 700,
        ..base.clone()
    };

    // Only the graph, as a second browser source next to the values.
    let verlauf = DesignConfig {
        template: s("verlauf"),
        elements: elements(&["graph"], &["graph"]),
        title: String::new(),
        colors: messgeraet.colors.clone(),
        bg_opacity: 0.82,
        border_opacity: 0.07,
        border_width: 1.0,
        radius: 14.0,
        font_text: s("system"),
        font_numbers: s("b612"),
        graph: GraphCfg {
            series: vec![s("v"), s("i")],
            seconds: 20.0,
            width: 520.0,
            height: 180.0,
            line: 2.0,
            fill: false,
            axes: true,
            legend: true,
            setpoints: true,
        },
        ..base.clone()
    };

    let leiste = DesignConfig {
        template: s("leiste"),
        layout: Layout::Bar,
        elements: elements(&["mode", "alarms", "v", "i", "p", "graph"], &[]),
        accent: Accent::None,
        radius: 14.0,
        graph: GraphCfg { height: 48.0, width: 220.0, ..GraphCfg::default() },
        ..base.clone()
    };

    let minimal = DesignConfig {
        template: s("minimal"),
        layout: Layout::Minimal,
        elements: elements(&["mode", "alarms", "v", "i", "p"], &[]),
        title: String::new(),
        auto_units: true,
        colors: Colors { text: s("#ffffff"), muted: s("#e1e5ea"), ..Colors::default() },
        bg_opacity: 0.0,
        border_width: 0.0,
        shadow: false,
        font_numbers: s("b612"),
        weight: 700,
        ..base.clone()
    };

    let seitenleiste = DesignConfig {
        template: s("seitenleiste"),
        layout: Layout::Column,
        elements: elements(
            &["mode", "alarms", "v", "i", "p", "graph"],
            &["logo", "title", "mode", "alarms", "v", "i", "p", "set", "graph"],
        ),
        title: String::new(),
        labels: true,
        accent: Accent::None,
        auto_units: true,
        colors: messgeraet.colors.clone(),
        bg_opacity: 0.8,
        radius: 16.0,
        font_numbers: s("b612"),
        graph: GraphCfg { height: 56.0, width: 160.0, ..GraphCfg::default() },
        ..base.clone()
    };

    let labornetzteil = DesignConfig {
        template: s("labornetzteil"),
        layout: Layout::Bench,
        elements: elements(
            &["title", "mode", "alarms", "v", "i", "set"],
            &["logo", "title", "model", "mode", "alarms", "v", "i", "p", "set", "dmm", "graph"],
        ),
        title: s("Labornetzteil"),
        labels: true,
        accent: Accent::None,
        colors: Colors {
            v: s("#ff5a1f"),
            i: s("#3dff7a"),
            p: s("#ffc21a"),
            dmm: s("#5ad1ff"),
            cv: s("#3dff7a"),
            cc: s("#ff5a1f"),
            alarm: s("#ff3b30"),
            text: s("#e9eaec"),
            muted: s("#9ea3ab"),
            bg: s("#2b2d31"),
            border: s("#000000"),
        },
        bg_opacity: 1.0,
        border_opacity: 0.7,
        border_width: 1.0,
        radius: 12.0,
        glow: true,
        font_text: s("din"),
        font_numbers: s("seg7"),
        weight: 700,
        graph: GraphCfg { series: vec![s("v"), s("i")], height: 60.0, ..GraphCfg::default() },
        ..base.clone()
    };

    let neon = DesignConfig {
        template: s("neon"),
        elements: elements(
            &["mode", "alarms", "v", "i", "p", "graph"],
            &["logo", "title", "mode", "alarms", "set", "v", "i", "p", "graph", "dmm"],
        ),
        title: s("Live · Netzteil"),
        labels: true,
        auto_units: true,
        colors: Colors {
            v: s("#00f0ff"),
            i: s("#ff2bd6"),
            p: s("#fff200"),
            dmm: s("#b026ff"),
            cv: s("#39ff14"),
            cc: s("#ff2bd6"),
            alarm: s("#ff3864"),
            text: s("#ffffff"),
            muted: s("#d3b8ff"),
            bg: s("#0d0221"),
            border: s("#ff2bd6"),
        },
        bg_opacity: 0.82,
        border_opacity: 1.0,
        border_width: 3.0,
        radius: 22.0,
        glow: true,
        font_text: s("din"),
        font_numbers: s("din"),
        weight: 700,
        graph: GraphCfg {
            series: vec![s("v"), s("i"), s("p")],
            height: 64.0,
            line: 2.5,
            fill: true,
            ..GraphCfg::default()
        },
        ..base.clone()
    };

    vec![
        Template {
            id: "messgeraet",
            name: "Messgerät",
            description: "Wie ein Messgerät: U | I | P groß nebeneinander, darunter CV/CC und Sollwerte.",
            cfg: messgeraet,
        },
        Template {
            id: "verlauf",
            name: "Verlauf",
            description: "Nur der Graph mit Achsen und Soll-Linien – als zweite Quelle neben den Zahlen.",
            cfg: verlauf,
        },
        Template {
            id: "klassisch",
            name: "Klassisch",
            description: "Die bewährte Kachel: große Zahlen untereinander, Mini-Graph.",
            cfg: base,
        },
        Template {
            id: "leiste",
            name: "Leiste",
            description: "Alles in einer Zeile – für den unteren Bildrand.",
            cfg: leiste,
        },
        Template {
            id: "minimal",
            name: "Minimal",
            description: "Ohne Hintergrund, große Zahlen mit Kontur – direkt übers Video.",
            cfg: minimal,
        },
        Template {
            id: "seitenleiste",
            name: "Seitenleiste",
            description: "Schmal und hoch – für den linken oder rechten Bildrand.",
            cfg: seitenleiste,
        },
        Template {
            id: "labornetzteil",
            name: "Labornetzteil",
            description: "Wie die Front eines Netzteils: leuchtende 7-Segment-Ziffern.",
            cfg: labornetzteil,
        },
        Template { id: "neon", name: "Neon", description: "Kräftige Farben, Leuchten und dicker Rahmen.", cfg: neon },
    ]
}

pub fn template(id: &str) -> Option<Template> {
    templates().into_iter().find(|t| t.id == id)
}

// ---------------------------------------------------------------- designs

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Design {
    pub id: String,
    pub name: String,
    pub cfg: DesignConfig,
}

/// Where a new design starts from.
pub enum Source<'a> {
    Template(&'a str),
    CopyOf(&'a str),
}

#[derive(Debug, PartialEq)]
pub enum DesignError {
    NotFound,
    /// Not allowed; the text says why (in German, for the studio).
    Refused(String),
    Io(String),
}

impl std::fmt::Display for DesignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DesignError::NotFound => write!(f, "Dieses Design gibt es nicht (mehr)."),
            DesignError::Refused(m) => write!(f, "{m}"),
            DesignError::Io(m) => write!(f, "Speichern fehlgeschlagen: {m}"),
        }
    }
}

/// `designs.json` on disk.
#[derive(Serialize)]
struct DesignFile<'a> {
    version: u32,
    rev: u64,
    default_id: &'a str,
    designs: &'a [Design],
}

/// URL-safe id from a name: `Mein Overlay!` → `mein-overlay`.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().to_lowercase().chars() {
        let mapped = match c {
            'ä' => "ae",
            'ö' => "oe",
            'ü' => "ue",
            'ß' => "ss",
            c if c.is_ascii_alphanumeric() => {
                out.push(c);
                continue;
            }
            _ => "-",
        };
        if mapped == "-" {
            if !out.is_empty() && !out.ends_with('-') {
                out.push('-');
            }
        } else {
            out.push_str(mapped);
        }
    }
    let mut s: String = out.chars().take(MAX_ID).collect();
    while s.ends_with('-') {
        s.pop();
    }
    if s.is_empty() { "design".into() } else { s }
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID
        && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `slug`, or `slug-2`, `slug-3` … if taken.
fn unique_id(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_string();
    }
    for n in 2.. {
        let suffix = format!("-{n}");
        let mut stem: String = base.chars().take(MAX_ID - suffix.len()).collect();
        while stem.ends_with('-') {
            stem.pop();
        }
        let id = format!("{stem}{suffix}");
        if !taken(&id) {
            return id;
        }
    }
    unreachable!()
}

/// The app's data folder: `~/Library/Application Support/PowerMeter` on
/// macOS, `%APPDATA%\PowerMeter` on Windows, `$XDG_DATA_HOME/powermeter`
/// or `~/.local/share/powermeter` elsewhere.
pub fn data_dir() -> PathBuf {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let home = || env("HOME").or_else(|| env("USERPROFILE")).unwrap_or_else(|| PathBuf::from("."));
    if cfg!(target_os = "windows") {
        env("APPDATA")
            .or_else(|| env("USERPROFILE").map(|p| p.join("AppData").join("Roaming")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("PowerMeter")
    } else if cfg!(target_os = "macos") {
        home().join("Library").join("Application Support").join("PowerMeter")
    } else {
        env("XDG_DATA_HOME").unwrap_or_else(|| home().join(".local").join("share")).join("powermeter")
    }
}

/// All designs, which one `/overlay` shows, and a revision that changes
/// with every edit (the overlay reloads its design when it does).
pub struct DesignStore {
    dir: PathBuf,
    designs: Vec<Design>,
    default_id: String,
    rev: u64,
    /// Problem found while loading, for the user.
    pub note: Option<String>,
}

impl DesignStore {
    /// The store in the app's data folder.
    pub fn open_default() -> Self {
        Self::open(data_dir())
    }

    /// Loads `dir/designs.json`. A missing file starts with the design
    /// `standard`; an unreadable one is moved aside to `designs.json.bak`
    /// first, so nothing the user made is ever lost.
    pub fn open(dir: PathBuf) -> Self {
        let mut st = DesignStore { dir, designs: Vec::new(), default_id: String::new(), rev: 0, note: None };
        let path = st.path();
        match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(v) => st.load_value(&v),
                Err(e) => st.note = Some(st.move_aside(&format!("designs.json ist beschädigt ({e})"))),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => st.note = Some(st.move_aside(&format!("designs.json lässt sich nicht lesen ({e})"))),
        }
        if st.designs.is_empty() {
            let cfg = templates().swap_remove(0).cfg;
            st.designs.push(Design { id: "standard".into(), name: "Standard".into(), cfg });
            st.default_id = "standard".into();
            if let Err(e) = st.save() {
                st.note.get_or_insert(format!("Designs: {e}"));
            }
        }
        st
    }

    fn path(&self) -> PathBuf {
        self.dir.join(FILE_NAME)
    }

    /// Renames a broken file to `.bak` (or `.bak2`, … if that exists).
    fn move_aside(&self, what: &str) -> String {
        let path = self.path();
        let mut bak = self.dir.join(format!("{FILE_NAME}.bak"));
        let mut n = 2;
        while bak.exists() {
            bak = self.dir.join(format!("{FILE_NAME}.bak{n}"));
            n += 1;
        }
        match fs::rename(&path, &bak) {
            Ok(()) => format!("{what} – gesichert als {}, neu angefangen.", bak.display()),
            Err(e) => format!("{what} – Sichern fehlgeschlagen: {e}"),
        }
    }

    fn load_value(&mut self, v: &Value) {
        self.rev = v.get("rev").and_then(Value::as_u64).unwrap_or(0);
        let list = v.get("designs").and_then(Value::as_array).cloned().unwrap_or_default();
        for d in list.iter().take(MAX_DESIGNS) {
            let name = clean_text(d.get("name").and_then(Value::as_str).unwrap_or(""), MAX_NAME);
            let name = if name.is_empty() { "Design".to_string() } else { name };
            let id = d.get("id").and_then(Value::as_str).filter(|id| valid_id(id)).map(str::to_string);
            let id = unique_id(&id.unwrap_or_else(|| slugify(&name)), |x| self.designs.iter().any(|d| d.id == x));
            let cfg = DesignConfig::from_value(d.get("cfg").unwrap_or(&Value::Null));
            self.designs.push(Design { id, name, cfg });
        }
        let want = v.get("default_id").and_then(Value::as_str).unwrap_or("");
        self.default_id = match self.designs.iter().find(|d| d.id == want).or(self.designs.first()) {
            Some(d) => d.id.clone(),
            None => String::new(),
        };
    }

    /// Writes `designs.json` atomically (temp file + rename).
    fn save(&self) -> Result<(), String> {
        let file = DesignFile { version: 1, rev: self.rev, default_id: &self.default_id, designs: &self.designs };
        let json = serde_json::to_vec_pretty(&file).map_err(|e| e.to_string())?;
        fs::create_dir_all(&self.dir).map_err(|e| format!("{}: {e}", self.dir.display()))?;
        write_atomic(&self.path(), &json).map_err(|e| format!("{}: {e}", self.path().display()))
    }

    fn changed(&mut self) -> Result<(), DesignError> {
        self.rev += 1;
        self.save().map_err(DesignError::Io)
    }

    pub fn assets_dir(&self) -> PathBuf {
        self.dir.join(ASSET_DIR)
    }

    pub fn rev(&self) -> u64 {
        self.rev
    }

    pub fn default_id(&self) -> &str {
        &self.default_id
    }

    pub fn list(&self) -> &[Design] {
        &self.designs
    }

    pub fn get(&self, id: &str) -> Option<&Design> {
        self.designs.iter().find(|d| d.id == id)
    }

    pub fn default_design(&self) -> &Design {
        self.get(&self.default_id).unwrap_or(&self.designs[0])
    }

    pub fn create(&mut self, name: &str, from: Source<'_>) -> Result<Design, DesignError> {
        if self.designs.len() >= MAX_DESIGNS {
            return Err(DesignError::Refused(format!(
                "Höchstens {MAX_DESIGNS} Designs – lösch erst eins, das du nicht mehr brauchst."
            )));
        }
        let cfg = match from {
            Source::Template(t) => {
                template(t).ok_or_else(|| DesignError::Refused(format!("Unbekannte Vorlage „{t}“.")))?.cfg
            }
            Source::CopyOf(id) => self.get(id).ok_or(DesignError::NotFound)?.cfg.clone(),
        };
        let name = clean_text(name, MAX_NAME);
        let name = if name.is_empty() { "Mein Overlay".to_string() } else { name };
        let id = unique_id(&slugify(&name), |x| self.get(x).is_some());
        let d = Design { id, name, cfg };
        self.designs.push(d.clone());
        self.changed()?;
        Ok(d)
    }

    /// Saves a new name and/or look. The id (and with it the OBS URL)
    /// never changes.
    pub fn update(&mut self, id: &str, name: Option<&str>, cfg: Option<DesignConfig>) -> Result<Design, DesignError> {
        let d = self.designs.iter_mut().find(|d| d.id == id).ok_or(DesignError::NotFound)?;
        if let Some(n) = name.map(|n| clean_text(n, MAX_NAME)).filter(|n| !n.is_empty()) {
            d.name = n;
        }
        if let Some(mut c) = cfg {
            c.sanitize();
            d.cfg = c;
        }
        let d = d.clone();
        self.changed()?;
        Ok(d)
    }

    pub fn delete(&mut self, id: &str) -> Result<(), DesignError> {
        if self.get(id).is_none() {
            return Err(DesignError::NotFound);
        }
        if id == self.default_id {
            return Err(DesignError::Refused(
                "Das Standard-Design lässt sich nicht löschen. Mach erst ein anderes zum Standard.".into(),
            ));
        }
        self.designs.retain(|d| d.id != id);
        self.changed()
    }

    pub fn set_default(&mut self, id: &str) -> Result<(), DesignError> {
        if self.get(id).is_none() {
            return Err(DesignError::NotFound);
        }
        self.default_id = id.to_string();
        self.changed()
    }

    /// Stores an uploaded image under a name made from its content and
    /// returns that name. The type comes from the bytes, never from the
    /// client.
    pub fn store_asset(&self, bytes: &[u8]) -> Result<String, String> {
        if bytes.is_empty() {
            return Err("Die Datei ist leer.".into());
        }
        if bytes.len() > MAX_ASSET_BYTES {
            return Err("Das Bild ist größer als 5 MB.".into());
        }
        let kind = ImageKind::detect(bytes).ok_or("Nur PNG, JPEG, GIF, WEBP oder SVG gehen als Logo.")?;
        let name = format!("{}.{}", content_hash(bytes), kind.ext());
        let dir = self.assets_dir();
        let path = dir.join(&name);
        if !path.exists() {
            fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            write_atomic(&path, bytes).map_err(|e| e.to_string())?;
        }
        Ok(name)
    }

    /// Path of a stored asset; `None` for anything that isn't a name
    /// [`store_asset`](Self::store_asset) makes.
    pub fn asset_path(&self, name: &str) -> Option<PathBuf> {
        valid_asset_name(name).then(|| self.assets_dir().join(name))
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

// ----------------------------------------------------------------- assets

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageKind {
    Png,
    Jpeg,
    Gif,
    Webp,
    Svg,
}

impl ImageKind {
    pub const ALL: [ImageKind; 5] = [ImageKind::Png, ImageKind::Jpeg, ImageKind::Gif, ImageKind::Webp, ImageKind::Svg];

    /// From the magic bytes (SVG: UTF-8 text with an `<svg` element).
    pub fn detect(b: &[u8]) -> Option<ImageKind> {
        if b.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Some(ImageKind::Png);
        }
        if b.starts_with(&[0xff, 0xd8, 0xff]) {
            return Some(ImageKind::Jpeg);
        }
        if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
            return Some(ImageKind::Gif);
        }
        if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
            return Some(ImageKind::Webp);
        }
        let text = std::str::from_utf8(b).ok()?;
        let head = text.trim_start_matches('\u{feff}').trim_start();
        let start = head.get(..head.len().min(4096))?.to_ascii_lowercase();
        (start.starts_with('<') && start.contains("<svg")).then_some(ImageKind::Svg)
    }

    pub fn ext(self) -> &'static str {
        match self {
            ImageKind::Png => "png",
            ImageKind::Jpeg => "jpg",
            ImageKind::Gif => "gif",
            ImageKind::Webp => "webp",
            ImageKind::Svg => "svg",
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            ImageKind::Png => "image/png",
            ImageKind::Jpeg => "image/jpeg",
            ImageKind::Gif => "image/gif",
            ImageKind::Webp => "image/webp",
            ImageKind::Svg => "image/svg+xml",
        }
    }

    pub fn from_name(name: &str) -> Option<ImageKind> {
        let ext = name.rsplit_once('.')?.1;
        ImageKind::ALL.into_iter().find(|k| k.ext() == ext)
    }
}

/// 32 hex digits plus one of our extensions, nothing else.
pub fn valid_asset_name(name: &str) -> bool {
    match name.split_once('.') {
        Some((hash, _)) => {
            hash.len() == 32
                && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                && ImageKind::from_name(name).is_some()
        }
        None => false,
    }
}

/// Two FNV-1a passes with different seeds: 128 bits, plenty to name files.
fn content_hash(bytes: &[u8]) -> String {
    let fnv = |seed: u64| {
        let mut h = seed;
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h ^ bytes.len() as u64
    };
    format!("{:016x}{:016x}", fnv(0xcbf2_9ce4_8422_2325), fnv(0x6c62_272e_07bb_0142))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A fresh, empty folder under the system temp dir.
    pub fn temp_dir(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("powermeter-test-{}-{tag}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn slugs_are_url_safe_and_unique() {
        assert_eq!(slugify("Mein Overlay"), "mein-overlay");
        assert_eq!(slugify("  Müco's Größe!! "), "mueco-s-groesse");
        assert_eq!(slugify("///"), "design");
        assert_eq!(slugify(&"x".repeat(100)).len(), 40);
        assert!(valid_id(&slugify("Ünïcödé ✨ 2025")));

        let dir = temp_dir("slug");
        let mut st = DesignStore::open(dir.clone());
        let a = st.create("Mein Overlay", Source::Template("klassisch")).unwrap();
        let b = st.create("Mein Overlay", Source::Template("neon")).unwrap();
        let c = st.create("mein overlay", Source::CopyOf(&a.id)).unwrap();
        assert_eq!((a.id.as_str(), b.id.as_str(), c.id.as_str()), ("mein-overlay", "mein-overlay-2", "mein-overlay-3"));
        assert_eq!(c.cfg, a.cfg);
        let long = st.create(&"lang".repeat(20), Source::Template("klassisch")).unwrap();
        let long2 = st.create(&"lang".repeat(20), Source::Template("klassisch")).unwrap();
        assert!(long2.id.len() <= 40 && long2.id.ends_with("-2") && long.id != long2.id);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn hostile_input_is_clamped() {
        let v = json!({
            "layout": "spaceship",
            "accent": 7,
            "title": format!("  {}\nzwei  ", "T".repeat(500)),
            "colors": { "v": "red", "i": "#ABC", "p": "#12345g", "bg": "#0A0B0C", "text": "javascript:alert(1)" },
            "bg_opacity": 9,
            "border_width": -3,
            "radius": 1e300,
            "scale": "huge",
            "weight": 12345,
            "font_text": "seg7",
            "font_numbers": "Comic Sans",
            "decimals": { "v": 9, "i": "auto", "p": "x" },
            "graph": { "series": ["v", "v", "q", "p"], "seconds": 0, "height": 9999, "line": "thick" },
            "elements": [{ "key": "v", "on": true }, { "key": "evil", "on": true }, { "key": "v", "on": false }, 5],
            "logo": "../../etc/passwd",
            "logo_height": 5,
            "custom_css": "a".repeat(30_000) + "ü",
            "unknown_field": { "deep": [1, 2, 3] }
        });
        let c = DesignConfig::from_value(&v);
        assert_eq!(c.layout, Layout::Card);
        assert_eq!(c.accent, Accent::V);
        assert_eq!(c.title.chars().count(), 60);
        assert!(!c.title.contains('\n'));
        assert_eq!(c.colors.v, Colors::default().v);
        assert_eq!(c.colors.i, "#aabbcc");
        assert_eq!(c.colors.p, Colors::default().p);
        assert_eq!(c.colors.bg, "#0a0b0c");
        assert_eq!(c.colors.text, Colors::default().text);
        assert_eq!((c.bg_opacity, c.border_width, c.radius, c.scale), (1.0, 0.0, 40.0, 1.0));
        assert_eq!(c.weight, 900);
        assert_eq!((c.font_text.as_str(), c.font_numbers.as_str()), ("system", "mono"));
        assert_eq!(c.decimals, DecimalsCfg { v: Decimals::Fixed(4), i: Decimals::Auto, p: Decimals::Auto });
        assert_eq!(c.graph.series, vec!["v", "p"]);
        assert_eq!((c.graph.seconds, c.graph.height, c.graph.line), (5.0, 240.0, 2.0));
        assert_eq!(c.elements.len(), ELEMENT_KEYS.len());
        assert_eq!(c.elements[0], Element { key: "v".into(), on: true });
        assert!(c.elements[1..].iter().all(|e| !e.on));
        assert_eq!(c.logo, None);
        assert_eq!(c.logo_height, 16.0);
        assert!(c.custom_css.len() <= MAX_CUSTOM_CSS);
        // a config that isn't even an object
        assert_eq!(DesignConfig::from_value(&json!("nope")), DesignConfig::default());
        // round trip of a sane config is lossless
        for t in templates() {
            let back = DesignConfig::from_value(&serde_json::to_value(&t.cfg).unwrap());
            assert_eq!(back, t.cfg, "{}", t.id);
        }
    }

    #[test]
    fn partial_and_odd_files_load() {
        let dir = temp_dir("partial");
        let file = json!({
            "rev": 7,
            "default_id": "gibts-nicht",
            "designs": [
                { "id": "bar", "name": "Leiste unten", "cfg": { "layout": "bar", "title": "Hallo" } },
                { "id": "Bad ID!", "name": "Zweites", "cfg": null },
                { "id": "bar", "name": "Doppelt" },
                "kaputt"
            ]
        });
        fs::write(dir.join("designs.json"), file.to_string()).unwrap();
        let st = DesignStore::open(dir.clone());
        assert_eq!(st.note, None);
        assert_eq!(st.rev(), 7);
        let ids: Vec<&str> = st.list().iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["bar", "zweites", "bar-2", "design"]);
        assert_eq!(st.default_id(), "bar");
        let bar = st.get("bar").unwrap();
        assert_eq!((bar.cfg.layout, bar.cfg.title.as_str()), (Layout::Bar, "Hallo"));
        assert_eq!(bar.cfg.colors, Colors::default());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn broken_file_is_kept_as_backup() {
        let dir = temp_dir("broken");
        fs::write(dir.join("designs.json"), b"{ das ist kein json").unwrap();
        let st = DesignStore::open(dir.clone());
        assert!(st.note.as_deref().unwrap_or("").contains("gesichert"), "{:?}", st.note);
        assert_eq!(fs::read(dir.join("designs.json.bak")).unwrap(), b"{ das ist kein json");
        assert_eq!(st.list().len(), 1);
        assert_eq!(st.default_id(), "standard");
        // a second broken file doesn't overwrite the first backup
        fs::write(dir.join("designs.json"), b"auch kaputt").unwrap();
        let _ = DesignStore::open(dir.clone());
        assert_eq!(fs::read(dir.join("designs.json.bak2")).unwrap(), b"auch kaputt");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_and_reload_round_trip() {
        let dir = temp_dir("roundtrip");
        let mut st = DesignStore::open(dir.clone());
        assert_eq!(st.default_id(), "standard");
        assert!(dir.join("designs.json").exists(), "first start writes the file");
        let rev0 = st.rev();
        let d = st.create("Neon groß", Source::Template("neon")).unwrap();
        let mut cfg = d.cfg.clone();
        cfg.title = "MÜCO REPAIR · NETZTEIL".into();
        cfg.scale = 1.5;
        st.update(&d.id, Some("Neon riesig"), Some(cfg.clone())).unwrap();
        st.set_default(&d.id).unwrap();
        assert_eq!(st.rev(), rev0 + 3);
        assert!(!dir.join("designs.tmp").exists(), "no temp file left behind");

        let again = DesignStore::open(dir.clone());
        assert_eq!(again.rev(), st.rev());
        assert_eq!(again.default_id(), "neon-gross");
        let d2 = again.get("neon-gross").unwrap();
        assert_eq!(d2.name, "Neon riesig");
        assert_eq!(d2.cfg, cfg);
        assert_eq!(again.list().len(), 2);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn default_design_is_protected() {
        let dir = temp_dir("default");
        let mut st = DesignStore::open(dir.clone());
        assert!(matches!(st.delete("standard"), Err(DesignError::Refused(_))));
        assert_eq!(st.delete("nope"), Err(DesignError::NotFound));
        assert_eq!(st.set_default("nope"), Err(DesignError::NotFound));
        let d = st.create("Zweites", Source::Template("leiste")).unwrap();
        st.set_default(&d.id).unwrap();
        st.delete("standard").unwrap();
        assert!(matches!(st.delete(&d.id), Err(DesignError::Refused(_))));
        assert_eq!(st.list().len(), 1);
        assert!(matches!(st.create("x", Source::Template("gibts-nicht")), Err(DesignError::Refused(_))));
        assert_eq!(st.create("x", Source::CopyOf("gibts-nicht")), Err(DesignError::NotFound));
        for k in 0..MAX_DESIGNS - 1 {
            st.create(&format!("D{k}"), Source::Template("klassisch")).unwrap();
        }
        assert!(matches!(st.create("zu viel", Source::Template("klassisch")), Err(DesignError::Refused(_))));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_overlay_params_map_onto_klassisch() {
        let q = |s: &str| -> HashMap<String, String> {
            s.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.into(), v.into())).collect()
        };
        assert!(!is_legacy_query(&q("")));
        let c = DesignConfig::from_legacy_query(&q("layout=bar&show=v,i,p,mode,graph,dmm"));
        assert_eq!(c.layout, Layout::Bar);
        assert!(c.is_on("v") && c.is_on("graph") && c.is_on("dmm") && c.is_on("title"));
        assert!(!c.is_on("set") && !c.is_on("energy"));
        let c = DesignConfig::from_legacy_query(&q("show=dmm&scale=1.4"));
        assert!(c.is_on("dmm") && !c.is_on("v") && !c.is_on("title") && !c.is_on("alarms"));
        assert_eq!(c.scale, 1.4);
        let c = DesignConfig::from_legacy_query(&q("graph=v,i,p&accent=p&bg=0.3&scale=99"));
        assert_eq!(c.graph.series, vec!["v", "i", "p"]);
        assert_eq!((c.accent, c.bg_opacity, c.scale), (Accent::P, 0.3, 3.0));
        assert!(c.is_on("set"), "default elements stay");
    }

    #[test]
    fn images_are_recognised_by_their_bytes() {
        assert_eq!(ImageKind::detect(b"\x89PNG\r\n\x1a\n...."), Some(ImageKind::Png));
        assert_eq!(ImageKind::detect(&[0xff, 0xd8, 0xff, 0xe0]), Some(ImageKind::Jpeg));
        assert_eq!(ImageKind::detect(b"GIF89a.."), Some(ImageKind::Gif));
        assert_eq!(ImageKind::detect(b"RIFF\0\0\0\0WEBPVP8 "), Some(ImageKind::Webp));
        let svg = "\u{feff}<?xml version=\"1.0\"?>\n<svg xmlns=\"http://www.w3.org/2000/svg\"/>";
        assert_eq!(ImageKind::detect(svg.as_bytes()), Some(ImageKind::Svg));
        assert_eq!(ImageKind::detect(b"<html><script>alert(1)</script></html>"), None);
        assert_eq!(ImageKind::detect(b"MZ\x90\0"), None);
        assert!(valid_asset_name(&format!("{}.png", "a".repeat(32))));
        for bad in ["../x.png", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA.png", "x.png", &format!("{}.exe", "a".repeat(32))] {
            assert!(!valid_asset_name(bad), "{bad}");
        }
        let dir = temp_dir("assets");
        let st = DesignStore::open(dir.clone());
        let name = st.store_asset(b"GIF89a-logo").unwrap();
        assert!(valid_asset_name(&name) && name.ends_with(".gif"));
        assert_eq!(st.store_asset(b"GIF89a-logo").unwrap(), name, "same bytes, same name");
        assert_eq!(fs::read(st.asset_path(&name).unwrap()).unwrap(), b"GIF89a-logo");
        assert!(st.store_asset(b"plain text").is_err());
        assert_eq!(st.asset_path("../designs.json"), None);
        fs::remove_dir_all(dir).unwrap();
    }
}
