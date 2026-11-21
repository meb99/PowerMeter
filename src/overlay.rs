//! Local web server for the OBS overlay and the Overlay-Studio.
//!
//! OBS → Quelle hinzufügen → Browser → URL `http://127.0.0.1:8765/overlay`
//! (the default design) or `http://127.0.0.1:8765/o/<design>`. The pages
//! subscribe to `/events` (Server-Sent Events) and get a fresh reading up
//! to 60 times per second. `/studio` edits the designs.
//!
//! Reading is open (OBS may run on another computer when "Overlay im
//! Netzwerk freigeben" is on). Anything that changes something only works
//! from this computer, from the studio page itself (see [`check_write`]).

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::LazyLock;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use futures_util::stream::Stream;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::designs::{self, Design, DesignConfig, DesignError, DesignStore, ImageKind, SharedDesigns, Source};
use crate::format;
use crate::model::{ConnState, EventKind, Shared, Store, first_index_after};

const OVERLAY_HTML: &str = include_str!("../assets/overlay.html");
const INDEX_HTML: &str = include_str!("../assets/index.html");
const STUDIO_HTML: &str = include_str!("../assets/studio.html");
const OVERLAY_JS: &str = include_str!("../assets/overlay.js");
/// B612 Mono (SIL OFL 1.1, see `assets/fonts/OFL.txt`), also used by the app.
pub const B612_MONO_BOLD: &[u8] = include_bytes!("../assets/fonts/B612Mono-Bold.ttf");
const B612_MONO_REGULAR: &[u8] = include_bytes!("../assets/fonts/B612Mono-Regular.ttf");
const FONT_LICENSE: &str = include_str!("../assets/fonts/OFL.txt");

/// The renderer with the font list of [`designs::FONTS`] in front, so Rust
/// stays the one place that knows the fonts.
static OVERLAY_SCRIPT: LazyLock<String> = LazyLock::new(|| {
    let fonts = serde_json::to_string(&designs::FONTS).unwrap_or_else(|_| "[]".into());
    format!("window.__POWERMETER_FONTS__ = {fonts};\n{OVERLAY_JS}")
});

const NOT_FOUND_HTML: &str = r#"<!doctype html>
<html lang="de"><head><meta charset="utf-8"><title>Overlay nicht gefunden</title>
<style>
  html, body { margin: 0; background: transparent; font-family: -apple-system, "Segoe UI", Arial, sans-serif; }
  .box { display: inline-block; margin: 8px; padding: 14px 18px; border-radius: 12px; max-width: 460px;
         background: rgba(12,14,18,0.85); color: #f5f7fa; font-size: 15px; line-height: 1.4; }
  b { color: #ffd60a; } a { color: #4cc9f0; }
</style></head>
<body><div class="box"><b>Dieses Overlay gibt es nicht.</b><br>
Die URL passt zu keinem Design. Öffne das <a href="/studio">Overlay-Studio</a>, wähle dein Design
und kopiere unter „In OBS einfügen“ die richtige URL.</div></body></html>"#;

/// Seconds of history in the overlay graph unless the page asks for more.
const SPARK_SECONDS: f64 = 20.0;
const SPARK_MAX_SECONDS: f64 = 60.0;
const SPARK_POINTS: usize = 480;
const JSON_LIMIT: usize = 64 * 1024;

#[derive(Serialize, Default)]
pub struct Snapshot {
    pub power_connected: bool,
    pub v: Option<f64>,
    pub i: Option<f64>,
    pub p: Option<f64>,
    pub v_text: String,
    pub i_text: String,
    pub p_text: String,
    /// Decimals the source really resolves (SPE: 2 / 3 / 2).
    pub v_decimals: usize,
    pub i_decimals: usize,
    pub p_decimals: usize,
    pub mode: &'static str,
    pub v_set: Option<f64>,
    pub i_set: Option<f64>,
    pub v_set_text: String,
    pub i_set_text: String,
    /// A controllable supply (OWON SPM/SPE) is connected.
    pub psu_connected: bool,
    /// "OWON SPE3103", or what else delivers the values.
    pub psu_model: String,
    /// The voltage set point was read from the supply ("Soll"), not typed
    /// in or learned.
    pub set_from_device: bool,
    /// The voltage set point is only learned ("≈ Soll").
    pub set_learned: bool,
    /// The current limit is only learned ("≈ Limit").
    pub i_set_learned: bool,
    /// Output state as reported by the supply.
    pub output_on: Option<bool>,
    /// "OVP", "OCP" … while a protection has switched the output off.
    pub protection: Option<String>,
    pub energy_text: String,
    pub charge_text: String,
    pub peak_i_text: String,
    pub dropout: bool,
    pub short: bool,
    pub dropouts: usize,
    pub last_event: Option<String>,
    pub dmm_connected: bool,
    pub dmm_value: Option<f64>,
    pub dmm_text: String,
    /// `dmm_text` split into number and unit ("3.3012", "V").
    pub dmm_value_text: String,
    pub dmm_unit: String,
    pub dmm_function: &'static str,
    pub dmm_tag: &'static str,
    /// Changes whenever a design changes; the overlay then reloads it.
    pub design_rev: u64,
    /// `[t, v, i, p]` rows, only sent a few times per second.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spark: Option<Vec<[f64; 4]>>,
}

impl Snapshot {
    /// `spark_secs`: seconds of graph history to include, `None` for none.
    pub fn build(s: &Store, dmm_digits: usize, spark_secs: Option<f64>) -> Self {
        let a = &s.power.analyzer;
        let res = s.power.resolution;
        let last = s.display_power();
        let dmm_last = s.dmm.samples.back().copied();
        let now = s.now();
        let fresh_power = last.filter(|l| now - l.t < 2.0);
        let v_set = a.v_set_effective(&s.analysis);
        let i_set = a.i_limit(&s.analysis);
        let psu = s.power.psu.as_ref();
        let last_event = a.events.iter().rev().find(|e| e.kind != EventKind::Marker).map(|e| {
            format!(
                "{} {} · min {}",
                e.kind.label(),
                format::duration(e.duration(now)),
                format::fixed(e.v_min, 2) + " V"
            )
        });
        let spark = spark_secs.map(|secs| {
            let start = first_index_after(&s.power.samples, now - secs, |x| x.t);
            let rows: Vec<[f64; 4]> = s.power.samples.range(start..).map(|x| [x.t - now, x.v, x.i, x.p()]).collect();
            spark_decimate(&rows, SPARK_POINTS / 4)
                .into_iter()
                .map(|r| [round(r[0], 3), round(r[1], 4), round(r[2], 5), round(r[3], 4)])
                .collect()
        });
        let p_dec = fresh_power.map_or(res.p, |l| res.p_for(l.p()));
        let psu_model = match (psu, &s.power.conn) {
            (Some(p), _) if !p.model.is_empty() => p.model.clone(),
            (_, ConnState::Connected(info)) if info != "verbunden" => info.clone(),
            _ => String::new(),
        };
        let (dmm_text, dmm_value_text, dmm_unit) = match dmm_last.filter(|d| now - d.t < 3.0) {
            None => ("-----".to_string(), "-----".to_string(), s.dmm.function.unit().to_string()),
            Some(d) if d.value.is_nan() => ("OL".into(), "OL".into(), s.dmm.function.unit().to_string()),
            Some(d) => {
                let (m, p) = format::scale(d.value);
                let num = format::sig(m, dmm_digits);
                let unit = format!("{p}{}", s.dmm.function.unit());
                (format!("{num} {unit}"), num, unit)
            }
        };
        Snapshot {
            power_connected: s.power.conn.is_connected(),
            v: fresh_power.map(|l| l.v),
            i: fresh_power.map(|l| l.i),
            p: fresh_power.map(|l| l.p()),
            v_text: fresh_power.map_or(format::placeholder(res.v, 2), |l| format::fixed(l.v, res.v)),
            i_text: fresh_power.map_or(format::placeholder(res.i, 1), |l| format::fixed(l.i, res.i)),
            p_text: fresh_power.map_or(format::placeholder(res.p, 2), |l| format::fixed(l.p(), p_dec)),
            v_decimals: res.v,
            i_decimals: res.i,
            p_decimals: p_dec,
            mode: if fresh_power.is_some() { a.mode.label() } else { "--" },
            v_set,
            i_set,
            v_set_text: v_set.map_or(String::new(), |v| format::fixed(v, 2)),
            i_set_text: i_set.map_or(String::new(), |i| format::fixed(i, 3)),
            psu_connected: s.power.psu_conn.is_connected(),
            psu_model,
            set_from_device: a.device_vset().is_some(),
            set_learned: a.v_set_is_learned(&s.analysis),
            i_set_learned: a.i_set_is_learned(&s.analysis),
            output_on: psu.and_then(|p| p.output_on),
            protection: psu.and_then(|p| p.protection_text()),
            energy_text: if a.energy_wh < 1.0 {
                format!("{:.2} mWh", a.energy_wh * 1000.0)
            } else {
                format!("{:.3} Wh", a.energy_wh)
            },
            charge_text: format!("{:.1} mAh", a.charge_ah * 1000.0),
            peak_i_text: if a.i.n > 0 { format::fixed(a.i.max, res.i.min(3)) + " A" } else { String::new() },
            dropout: a.active(EventKind::Dropout),
            short: a.active(EventKind::Short),
            dropouts: a.events.iter().filter(|e| e.kind == EventKind::Dropout).count(),
            last_event,
            dmm_connected: s.dmm.conn.is_connected(),
            dmm_value: dmm_last.map(|d| d.value).filter(|v| v.is_finite()),
            dmm_text,
            dmm_value_text,
            dmm_unit,
            dmm_function: s.dmm.function.label(),
            dmm_tag: s.dmm.function.tag(),
            design_rev: 0,
            spark,
        }
    }
}

/// Keeps, per bucket, the rows with the extreme voltage and current so dips
/// and current spikes survive in the small overlay graph.
fn spark_decimate(rows: &[[f64; 4]], buckets: usize) -> Vec<[f64; 4]> {
    if rows.len() <= buckets * 4 {
        return rows.to_vec();
    }
    let per = rows.len().div_ceil(buckets);
    let mut out = Vec::with_capacity(buckets * 4);
    for chunk in rows.chunks(per) {
        let mut idx = [0usize; 4];
        for (k, r) in chunk.iter().enumerate() {
            if r[1] < chunk[idx[0]][1] {
                idx[0] = k;
            }
            if r[1] > chunk[idx[1]][1] {
                idx[1] = k;
            }
            if r[2] < chunk[idx[2]][2] {
                idx[2] = k;
            }
            if r[2] > chunk[idx[3]][2] {
                idx[3] = k;
            }
        }
        idx.sort_unstable();
        let mut last = usize::MAX;
        for k in idx {
            if k != last {
                out.push(chunk[k]);
                last = k;
            }
        }
    }
    out
}

fn round(x: f64, decimals: i32) -> f64 {
    let f = 10f64.powi(decimals);
    (x * f).round() / f
}

/// `?spark=<seconds>`: 0 = no graph data, default 20 s, at most 60 s.
fn spark_param(q: &HashMap<String, String>) -> Option<f64> {
    match q.get("spark").and_then(|s| s.parse::<f64>().ok()) {
        Some(s) if s <= 0.0 || !s.is_finite() => None,
        Some(s) => Some(s.clamp(1.0, SPARK_MAX_SECONDS)),
        None => Some(SPARK_SECONDS),
    }
}

#[derive(Clone)]
struct AppState {
    store: Shared,
    designs: SharedDesigns,
    dmm_digits: usize,
    /// The port actually bound (the studio's URLs, the Origin check).
    port: u16,
}

impl AppState {
    fn design_url(&self, id: &str) -> String {
        format!("http://127.0.0.1:{}/o/{id}", self.port)
    }

    fn design_json(&self, d: &Design, ds: &DesignStore) -> Value {
        json!({
            "id": d.id,
            "name": d.name,
            "is_default": d.id == ds.default_id(),
            "url": self.design_url(&d.id),
            "rev": ds.rev(),
            "cfg": d.cfg,
        })
    }
}

/// Running server; dropping it shuts the server down.
pub struct OverlayServer {
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub error: Option<String>,
    /// The bound port (`addr` may ask for port 0).
    pub port: Option<u16>,
}

impl OverlayServer {
    pub fn start(store: Shared, designs: SharedDesigns, addr: SocketAddr, dmm_digits: usize) -> Self {
        let (tx, rx) = oneshot::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<u16, String>>();
        let thread = std::thread::Builder::new()
            .name("overlay-server".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                        return;
                    }
                };
                rt.block_on(async move {
                    let bound = tokio::net::TcpListener::bind(addr).await.and_then(|l| {
                        let port = l.local_addr()?.port();
                        Ok((l, port))
                    });
                    let (listener, port) = match bound {
                        Ok(x) => x,
                        Err(e) => {
                            let _ = ready_tx.send(Err(format!("Port {}: {e}", addr.port())));
                            return;
                        }
                    };
                    let _ = ready_tx.send(Ok(port));
                    let app = router(AppState { store, designs, dmm_digits, port });
                    let _ = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
                        .with_graceful_shutdown(async {
                            let _ = rx.await;
                        })
                        .await;
                });
            })
            .expect("spawn overlay server");
        let (error, port) = match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(port)) => (None, Some(port)),
            Ok(Err(e)) => (Some(e), None),
            Err(_) => (Some("Server startet nicht".into()), None),
        };
        Self { shutdown: Some(tx), thread: Some(thread), error, port }
    }
}

impl Drop for OverlayServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(|| async { page(INDEX_HTML) }))
        .route("/overlay", get(|| async { page(OVERLAY_HTML) }))
        .route("/o/{id}", get(design_page))
        .route("/overlay.js", get(overlay_js))
        .route("/studio", get(|| async { page(STUDIO_HTML) }))
        .route("/studio/{id}", get(|| async { page(STUDIO_HTML) }))
        .route("/fonts/{file}", get(font_file))
        .route("/api/live", get(live))
        .route("/events", get(events))
        .route("/api/designs", get(list_designs).post(create_design))
        .route("/api/designs/{id}", get(get_design).put(save_design).delete(delete_design))
        .route("/api/designs/{id}/default", post(make_default))
        .route("/api/overlay-design", get(overlay_design))
        .route("/api/templates", get(|| async { Json(designs::templates()) }))
        .route("/api/fonts", get(|| async { Json(designs::FONTS) }))
        .route("/api/assets", post(upload_asset).layer(DefaultBodyLimit::max(designs::MAX_ASSET_BYTES)))
        .route("/assets/{file}", get(asset_file))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .layer(DefaultBodyLimit::max(JSON_LIMIT))
        .with_state(state)
}

/// An HTML page that is never cached (the app may have been updated).
fn page(html: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        html,
    )
        .into_response()
}

fn error(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, [(header::CACHE_CONTROL, "no-store")], Json(json!({ "error": msg.into() }))).into_response()
}

fn design_error(e: DesignError) -> Response {
    let status = match e {
        DesignError::NotFound => StatusCode::NOT_FOUND,
        DesignError::Refused(_) => StatusCode::CONFLICT,
        DesignError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error(status, e.to_string())
}

/// Everything that changes something (PUT/POST/DELETE): only from this
/// computer, only with the studio's header and, if the browser names a
/// page (`Origin`), only from our own pages. A foreign website therefore
/// can't change designs, and preflights (OPTIONS) are never answered.
fn check_write(port: u16, req: &Request) -> Result<(), &'static str> {
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip().to_canonical());
    if !peer.is_some_and(|ip| ip.is_loopback()) {
        return Err("Änderungen gehen nur am Rechner, auf dem PowerMeter läuft.");
    }
    if req.headers().get("x-powermeter-studio").and_then(|v| v.to_str().ok()) != Some("1") {
        return Err("Änderungen gehen nur über das Overlay-Studio.");
    }
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        let ok = [format!("http://127.0.0.1:{port}"), format!("http://localhost:{port}")];
        if !ok.iter().any(|o| origin.as_bytes() == o.as_bytes()) {
            return Err("Abgelehnt: Die Anfrage kam von einer fremden Webseite.");
        }
    }
    Ok(())
}

async fn guard(State(st): State<AppState>, req: Request, next: Next) -> Response {
    if req.method() == Method::GET || req.method() == Method::HEAD {
        return next.run(req).await;
    }
    match check_write(st.port, &req) {
        Ok(()) => next.run(req).await,
        Err(msg) => error(StatusCode::FORBIDDEN, msg),
    }
}

/// A JSON request body, with German errors.
fn json_body(body: Result<Bytes, BytesRejection>) -> Result<Value, (StatusCode, &'static str)> {
    let bytes = body.map_err(|e| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            (StatusCode::PAYLOAD_TOO_LARGE, "Zu groß (höchstens 64 KB).")
        } else {
            (e.status(), "Die Daten kamen nicht vollständig an.")
        }
    })?;
    serde_json::from_slice(&bytes).map_err(|_| (StatusCode::BAD_REQUEST, "Ungültige Daten (kein JSON)."))
}

async fn design_page(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let known = st.designs.lock().unwrap().get(&id).is_some();
    if known { page(OVERLAY_HTML) } else { (StatusCode::NOT_FOUND, page(NOT_FOUND_HTML)).into_response() }
}

async fn overlay_js() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        OVERLAY_SCRIPT.as_str(),
    )
        .into_response()
}

async fn font_file(Path(file): Path<String>) -> Response {
    let (body, kind): (&'static [u8], &str) = match file.as_str() {
        "B612Mono-Bold.ttf" => (B612_MONO_BOLD, "font/ttf"),
        "B612Mono-Regular.ttf" => (B612_MONO_REGULAR, "font/ttf"),
        "OFL.txt" => (FONT_LICENSE.as_bytes(), "text/plain; charset=utf-8"),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, kind),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body,
    )
        .into_response()
}

fn snapshot(st: &AppState, spark: Option<f64>) -> Snapshot {
    let rev = st.designs.lock().unwrap().rev();
    let mut snap = {
        let s = st.store.lock().unwrap();
        Snapshot::build(&s, st.dmm_digits, spark)
    };
    snap.design_rev = rev;
    snap
}

async fn live(State(st): State<AppState>, Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
    let snap = snapshot(&st, spark_param(&q));
    ([("Access-Control-Allow-Origin", "*"), ("Cache-Control", "no-store")], Json(snap))
}

async fn events(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = async_stream(st, spark_param(&q));
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(5)))
}

fn async_stream(st: AppState, spark: Option<f64>) -> impl Stream<Item = Result<Event, Infallible>> {
    let start = (st, (u64::MAX, u64::MAX, u64::MAX), 0u32);
    futures_util::stream::unfold(start, move |(st, mut last, mut tick)| async move {
        let mut interval = tokio::time::interval(Duration::from_millis(16));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            tick = tick.wrapping_add(1);
            let with_spark = tick % 12 == 0; // ~5 Hz
            let rev = st.designs.lock().unwrap().rev();
            let json = {
                let s = st.store.lock().unwrap();
                let now = (s.power.seq, s.dmm.seq, rev);
                // send on new data or a design change, plus a spark refresh
                // even when idle so the "disconnected" state shows up
                if now == last && !with_spark {
                    continue;
                }
                last = now;
                let mut snap = Snapshot::build(&s, st.dmm_digits, spark.filter(|_| with_spark));
                snap.design_rev = rev;
                serde_json::to_string(&snap).unwrap_or_default()
            };
            return Some((Ok(Event::default().data(json)), (st, last, tick)));
        }
    })
}

async fn list_designs(State(st): State<AppState>) -> Response {
    let ds = st.designs.lock().unwrap();
    let list: Vec<Value> = ds
        .list()
        .iter()
        .map(|d| {
            json!({
                "id": d.id,
                "name": d.name,
                "layout": d.cfg.layout,
                "is_default": d.id == ds.default_id(),
                "url": st.design_url(&d.id),
            })
        })
        .collect();
    let body = json!({
        "rev": ds.rev(),
        "default_id": ds.default_id(),
        "port": st.port,
        "overlay_url": format!("http://127.0.0.1:{}/overlay", st.port),
        "note": ds.note,
        "designs": list,
    });
    ([(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

async fn get_design(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let ds = st.designs.lock().unwrap();
    match ds.get(&id) {
        Some(d) => ([(header::CACHE_CONTROL, "no-store")], Json(st.design_json(d, &ds))).into_response(),
        None => design_error(DesignError::NotFound),
    }
}

/// What `/overlay` shows: the old URL parameters on the `klassisch`
/// template, otherwise the default design.
async fn overlay_design(State(st): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let ds = st.designs.lock().unwrap();
    let body = if designs::is_legacy_query(&q) {
        json!({ "id": null, "name": "URL-Parameter", "rev": ds.rev(), "cfg": DesignConfig::from_legacy_query(&q) })
    } else {
        st.design_json(ds.default_design(), &ds)
    };
    ([(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

async fn create_design(State(st): State<AppState>, body: Result<Bytes, BytesRejection>) -> Response {
    let v = match json_body(body) {
        Ok(v) => v,
        Err((status, msg)) => return error(status, msg),
    };
    let name = v.get("name").and_then(Value::as_str).unwrap_or("");
    let source = match (v.get("copy_of").and_then(Value::as_str), v.get("template").and_then(Value::as_str)) {
        (Some(id), _) => Source::CopyOf(id),
        (None, Some(t)) => Source::Template(t),
        (None, None) => Source::Template("messgeraet"),
    };
    let mut ds = st.designs.lock().unwrap();
    match ds.create(name, source) {
        Ok(d) => (StatusCode::CREATED, Json(st.design_json(&d, &ds))).into_response(),
        Err(e) => design_error(e),
    }
}

async fn save_design(
    State(st): State<AppState>,
    Path(id): Path<String>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let v = match json_body(body) {
        Ok(v) => v,
        Err((status, msg)) => return error(status, msg),
    };
    let name = v.get("name").and_then(Value::as_str);
    let cfg = v.get("cfg").map(DesignConfig::from_value);
    let mut ds = st.designs.lock().unwrap();
    match ds.update(&id, name, cfg) {
        Ok(d) => Json(st.design_json(&d, &ds)).into_response(),
        Err(e) => design_error(e),
    }
}

async fn delete_design(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let mut ds = st.designs.lock().unwrap();
    match ds.delete(&id) {
        Ok(()) => Json(json!({ "ok": true, "rev": ds.rev(), "default_id": ds.default_id() })).into_response(),
        Err(e) => design_error(e),
    }
}

async fn make_default(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let mut ds = st.designs.lock().unwrap();
    match ds.set_default(&id) {
        Ok(()) => Json(json!({ "ok": true, "rev": ds.rev(), "default_id": ds.default_id() })).into_response(),
        Err(e) => design_error(e),
    }
}

async fn upload_asset(State(st): State<AppState>, body: Result<Bytes, BytesRejection>) -> Response {
    let bytes = match body {
        Ok(b) => b,
        Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return error(StatusCode::PAYLOAD_TOO_LARGE, "Das Bild ist größer als 5 MB.");
        }
        Err(e) => return error(e.status(), "Die Datei kam nicht vollständig an."),
    };
    if ImageKind::detect(&bytes).is_none() {
        return error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Nur PNG, JPEG, GIF, WEBP oder SVG gehen als Logo.");
    }
    let stored = st.designs.lock().unwrap().store_asset(&bytes);
    match stored {
        Ok(file) => Json(json!({ "url": format!("/assets/{file}"), "file": file })).into_response(),
        Err(e) => error(StatusCode::BAD_REQUEST, e),
    }
}

async fn asset_file(State(st): State<AppState>, Path(file): Path<String>) -> Response {
    let (Some(path), Some(kind)) = (st.designs.lock().unwrap().asset_path(&file), ImageKind::from_name(&file)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut resp = (
        [
            (header::CONTENT_TYPE, kind.mime()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            // the name is the content hash: it never changes
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        bytes,
    )
        .into_response();
    if kind == ImageKind::Svg {
        resp.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            header::HeaderValue::from_static("script-src 'none'; object-src 'none'"),
        );
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::designs::DesignStore;
    use crate::designs::tests::temp_dir;
    use crate::model::{PowerSample, Resolution};
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpStream};
    use std::sync::{Arc, Mutex};

    #[test]
    fn spark_keeps_current_spike_and_voltage_dip() {
        let rows: Vec<[f64; 4]> = (0..2000)
            .map(|k| {
                let v = if k == 700 { 5.0 } else { 19.0 };
                let i = if k == 1300 { 3.0 } else { 0.5 };
                [k as f64, v, i, v * i]
            })
            .collect();
        let d = spark_decimate(&rows, 60);
        assert!(d.len() <= 240);
        assert!(d.iter().any(|r| r[1] == 5.0));
        assert!(d.iter().any(|r| r[2] == 3.0));
        assert!(d.windows(2).all(|w| w[0][0] < w[1][0]));
    }

    #[test]
    fn readouts_follow_the_source_resolution() {
        let mut s = Store::default();
        let t = s.now();
        s.power.resolution = Resolution::SPM;
        s.push_power_sample_uncalibrated(PowerSample { t, v: 2.0, i: 0.909 });
        let snap = Snapshot::build(&s, 5, None);
        assert_eq!((snap.v_text.as_str(), snap.i_text.as_str(), snap.p_text.as_str()), ("2.00", "0.909", "1.82"));
        assert_eq!((snap.v_decimals, snap.i_decimals, snap.p_decimals), (2, 3, 2));
        s.power.resolution = Resolution::POWERMON;
        let snap = Snapshot::build(&s, 5, None);
        assert_eq!((snap.v_text.as_str(), snap.i_text.as_str()), ("2.000", "0.9090"));
        let empty = Snapshot::build(&Store::default(), 5, None);
        assert_eq!((empty.v_text.as_str(), empty.i_text.as_str()), ("--.---", "-.----"));
    }

    /// The real server on a free port, with designs in a temp folder.
    struct TestServer {
        _server: OverlayServer,
        port: u16,
        dir: std::path::PathBuf,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn server() -> TestServer {
        let dir = temp_dir("server");
        let designs = Arc::new(Mutex::new(DesignStore::open(dir.clone())));
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        let server = OverlayServer::start(Store::shared(), designs, addr, 5);
        assert_eq!(server.error, None);
        let port = server.port.unwrap();
        TestServer { _server: server, port, dir }
    }

    struct Reply {
        status: u16,
        head: String,
        body: Vec<u8>,
    }

    impl Reply {
        fn json(&self) -> Value {
            serde_json::from_slice(&self.body).unwrap_or(Value::Null)
        }

        fn header(&self, name: &str) -> Option<String> {
            self.head.lines().find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_string())
            })
        }
    }

    /// One raw HTTP/1.1 request.
    fn http(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Reply {
        let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        s.write_all(req.as_bytes()).unwrap();
        s.write_all(body).unwrap();
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf);
        let split = buf.windows(4).position(|w| w == b"\r\n\r\n").expect("HTTP head");
        let head = String::from_utf8_lossy(&buf[..split]).to_string();
        let mut body = buf[split + 4..].to_vec();
        let status = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
        if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
            body = dechunk(&body);
        }
        Reply { status, head, body }
    }

    fn dechunk(mut b: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(nl) = b.windows(2).position(|w| w == b"\r\n") {
            let len = usize::from_str_radix(String::from_utf8_lossy(&b[..nl]).trim(), 16).unwrap_or(0);
            if len == 0 {
                break;
            }
            out.extend_from_slice(&b[nl + 2..nl + 2 + len]);
            b = &b[nl + 4 + len..];
        }
        out
    }

    const STUDIO: (&str, &str) = ("X-PowerMeter-Studio", "1");
    const JSON: (&str, &str) = ("Content-Type", "application/json");

    fn rev(port: u16) -> u64 {
        http(port, "GET", "/api/live?spark=0", &[], b"").json()["design_rev"].as_u64().unwrap()
    }

    #[test]
    fn designs_api_round_trip() {
        let t = server();
        let p = t.port;
        let list = http(p, "GET", "/api/designs", &[], b"").json();
        assert_eq!(list["default_id"], "standard");
        assert_eq!(list["designs"][0]["url"], format!("http://127.0.0.1:{p}/o/standard"));
        assert_eq!(list["designs"][0]["layout"], "meter");

        let rev0 = rev(p);
        let r = http(p, "POST", "/api/designs", &[STUDIO, JSON], br#"{"name":"Leiste unten","template":"leiste"}"#);
        assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
        assert_eq!(r.json()["id"], "leiste-unten");
        assert_eq!(rev(p), rev0 + 1, "design_rev follows every change");

        let mut cfg = r.json()["cfg"].clone();
        cfg["title"] = json!("MÜCO REPAIR · NETZTEIL");
        cfg["colors"]["v"] = json!("not a color");
        let body = json!({ "name": "Leiste", "cfg": cfg }).to_string();
        let r = http(p, "PUT", "/api/designs/leiste-unten", &[STUDIO, JSON], body.as_bytes());
        assert_eq!(r.status, 200);
        let saved = http(p, "GET", "/api/designs/leiste-unten", &[], b"").json();
        assert_eq!(saved["name"], "Leiste");
        assert_eq!(saved["cfg"]["title"], "MÜCO REPAIR · NETZTEIL");
        assert_eq!(saved["cfg"]["colors"]["v"], "#ffd60a", "bad color falls back");
        assert_eq!(rev(p), rev0 + 2);

        let r = http(p, "POST", "/api/designs", &[STUDIO, JSON], br#"{"name":"Kopie","copy_of":"leiste-unten"}"#);
        assert_eq!(r.json()["cfg"]["title"], "MÜCO REPAIR · NETZTEIL");

        // the default design can't go; another one can
        let r = http(p, "DELETE", "/api/designs/standard", &[STUDIO], b"");
        assert_eq!(r.status, 409);
        assert!(r.json()["error"].as_str().unwrap().contains("Standard"));
        assert_eq!(http(p, "POST", "/api/designs/kopie/default", &[STUDIO], b"").status, 200);
        assert_eq!(http(p, "DELETE", "/api/designs/standard", &[STUDIO], b"").status, 200);
        assert_eq!(http(p, "DELETE", "/api/designs/standard", &[STUDIO], b"").status, 404);
        let list = http(p, "GET", "/api/designs", &[], b"").json();
        assert_eq!(list["default_id"], "kopie");
        assert_eq!(list["designs"].as_array().unwrap().len(), 2);

        // the default design is what /overlay shows; old parameters still work
        assert_eq!(http(p, "GET", "/api/overlay-design", &[], b"").json()["id"], "kopie");
        let legacy = http(p, "GET", "/api/overlay-design?layout=bar&show=v,i", &[], b"").json();
        assert_eq!(legacy["cfg"]["layout"], "bar");
        assert_eq!(legacy["cfg"]["template"], "klassisch");

        let r = http(p, "POST", "/api/designs", &[STUDIO, JSON], br#"{"template":"gibts-nicht"}"#);
        assert_eq!(r.status, 409);
        let r = http(p, "PUT", "/api/designs/kopie", &[STUDIO, JSON], b"{kaputt");
        assert_eq!(r.status, 400);
        let big = format!(r#"{{"name":"{}"}}"#, "x".repeat(70_000));
        assert_eq!(http(p, "PUT", "/api/designs/kopie", &[STUDIO, JSON], big.as_bytes()).status, 413);
    }

    #[test]
    fn writes_need_the_studio() {
        let t = server();
        let p = t.port;
        let body = br#"{"name":"Fremd"}"#;
        let r = http(p, "POST", "/api/designs", &[JSON], body);
        assert_eq!(r.status, 403);
        assert!(r.json()["error"].as_str().unwrap().contains("Studio"));
        let evil = ("Origin", "http://evil.example");
        assert_eq!(http(p, "POST", "/api/designs", &[STUDIO, JSON, evil], body).status, 403);
        let other_port = ("Origin", "http://127.0.0.1:1");
        assert_eq!(http(p, "DELETE", "/api/designs/standard", &[STUDIO, other_port], b"").status, 403);
        // CORS preflight: never answered
        let pre = http(
            p,
            "OPTIONS",
            "/api/designs/standard",
            &[
                evil,
                ("Access-Control-Request-Method", "PUT"),
                ("Access-Control-Request-Headers", "x-powermeter-studio"),
            ],
            b"",
        );
        assert_eq!(pre.status, 403);
        assert_eq!(pre.header("access-control-allow-origin"), None);
        // our own page
        let own = format!("http://localhost:{p}");
        let r = http(p, "POST", "/api/designs", &[STUDIO, JSON, ("Origin", &own)], body);
        assert_eq!(r.status, 201);
        assert_eq!(r.header("access-control-allow-origin"), None);
        assert_eq!(http(p, "GET", "/api/designs", &[], b"").json()["designs"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn pages_and_assets() {
        let t = server();
        let p = t.port;
        for path in ["/", "/overlay", "/overlay?layout=bar&show=v,i,p", "/o/standard", "/studio", "/studio/standard"] {
            let r = http(p, "GET", path, &[], b"");
            assert_eq!(r.status, 200, "{path}");
            assert!(r.header("content-type").unwrap().starts_with("text/html"), "{path}");
        }
        let r = http(p, "GET", "/o/unknown", &[], b"");
        assert_eq!(r.status, 404);
        assert!(String::from_utf8_lossy(&r.body).contains("gibt es nicht"));
        let js = http(p, "GET", "/overlay.js", &[], b"");
        assert!(String::from_utf8_lossy(&js.body).starts_with("window.__POWERMETER_FONTS__ = [{"));
        let font = http(p, "GET", "/fonts/B612Mono-Bold.ttf", &[], b"");
        assert_eq!((font.status, font.header("content-type").as_deref()), (200, Some("font/ttf")));
        assert_eq!(font.body.len(), B612_MONO_BOLD.len());
        assert_eq!(http(p, "GET", "/fonts/../designs.json", &[], b"").status, 404);

        // uploads: type from the bytes, name from the server
        let r = http(p, "POST", "/api/assets", &[STUDIO], b"<html><script>alert(1)</script>");
        assert_eq!(r.status, 415);
        assert_eq!(http(p, "POST", "/api/assets", &[], b"\x89PNG\r\n\x1a\nxx").status, 403);
        let r = http(p, "POST", "/api/assets", &[STUDIO, ("Content-Type", "text/html")], b"\x89PNG\r\n\x1a\nxx");
        assert_eq!(r.status, 200);
        let file = r.json()["file"].as_str().unwrap().to_string();
        assert!(file.ends_with(".png") && designs::valid_asset_name(&file));
        let got = http(p, "GET", &format!("/assets/{file}"), &[], b"");
        assert_eq!(got.body, b"\x89PNG\r\n\x1a\nxx");
        assert_eq!(got.header("content-type").as_deref(), Some("image/png"));
        assert_eq!(got.header("x-content-type-options").as_deref(), Some("nosniff"));
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script></svg>"#;
        let file = http(p, "POST", "/api/assets", &[STUDIO], svg).json()["file"].as_str().unwrap().to_string();
        let got = http(p, "GET", &format!("/assets/{file}"), &[], b"");
        assert_eq!(got.header("content-type").as_deref(), Some("image/svg+xml"));
        assert!(got.header("content-security-policy").unwrap().contains("script-src 'none'"));
        assert_eq!(http(p, "GET", "/assets/..%2Fdesigns.json", &[], b"").status, 404);
        assert_eq!(http(p, "GET", &format!("/assets/{}.png", "0".repeat(32)), &[], b"").status, 404);
    }
}
