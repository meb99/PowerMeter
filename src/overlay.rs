//! Local web server for the OBS overlay.
//!
//! OBS → Quelle hinzufügen → Browser → URL `http://127.0.0.1:8765/overlay`.
//! The page subscribes to `/events` (Server-Sent Events) and gets a fresh
//! reading up to 60 times per second.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Json};
use axum::routing::get;
use futures_util::stream::Stream;
use serde::Serialize;
use tokio::sync::oneshot;

use crate::format;
use crate::model::{EventKind, Shared, Store, first_index_after};

const OVERLAY_HTML: &str = include_str!("../assets/overlay.html");
const INDEX_HTML: &str = include_str!("../assets/index.html");

/// Seconds of history in the overlay sparkline.
const SPARK_SECONDS: f64 = 20.0;
const SPARK_POINTS: usize = 240;

#[derive(Serialize, Default)]
pub struct Snapshot {
    pub power_connected: bool,
    pub v: Option<f64>,
    pub i: Option<f64>,
    pub p: Option<f64>,
    pub v_text: String,
    pub i_text: String,
    pub p_text: String,
    pub mode: &'static str,
    pub v_set: Option<f64>,
    pub i_set: Option<f64>,
    pub v_set_text: String,
    pub i_set_text: String,
    /// A controllable supply (OWON SPM) is connected.
    pub psu_connected: bool,
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
    pub dmm_function: &'static str,
    pub dmm_tag: &'static str,
    /// `[t, v, i, p]` rows, only sent a few times per second.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spark: Option<Vec<[f64; 4]>>,
}

impl Snapshot {
    pub fn build(s: &Store, dmm_digits: usize, with_spark: bool) -> Self {
        let a = &s.power.analyzer;
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
        let spark = with_spark.then(|| {
            let start = first_index_after(&s.power.samples, now - SPARK_SECONDS, |x| x.t);
            let rows: Vec<[f64; 4]> = s.power.samples.range(start..).map(|x| [x.t - now, x.v, x.i, x.p()]).collect();
            spark_decimate(&rows, SPARK_POINTS / 4)
                .into_iter()
                .map(|r| [round(r[0], 3), round(r[1], 4), round(r[2], 5), round(r[3], 4)])
                .collect()
        });
        Snapshot {
            power_connected: s.power.conn.is_connected(),
            v: fresh_power.map(|l| l.v),
            i: fresh_power.map(|l| l.i),
            p: fresh_power.map(|l| l.p()),
            v_text: fresh_power.map_or("--.---".into(), |l| format::fixed(l.v, 3)),
            i_text: fresh_power.map_or("-.----".into(), |l| format::fixed(l.i, 4)),
            p_text: fresh_power.map_or("--.--".into(), |l| format::fixed(l.p(), if l.p() >= 100.0 { 1 } else { 2 })),
            mode: if fresh_power.is_some() { a.mode.label() } else { "--" },
            v_set,
            i_set,
            v_set_text: v_set.map_or(String::new(), |v| format::fixed(v, 2)),
            i_set_text: i_set.map_or(String::new(), |i| format::fixed(i, 3)),
            psu_connected: s.power.psu_conn.is_connected(),
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
            peak_i_text: if a.i.n > 0 { format::fixed(a.i.max, 3) + " A" } else { String::new() },
            dropout: a.active(EventKind::Dropout),
            short: a.active(EventKind::Short),
            dropouts: a.events.iter().filter(|e| e.kind == EventKind::Dropout).count(),
            last_event,
            dmm_connected: s.dmm.conn.is_connected(),
            dmm_value: dmm_last.map(|d| d.value).filter(|v| v.is_finite()),
            dmm_text: dmm_last.filter(|d| now - d.t < 3.0).map_or("-----".into(), |d| {
                if d.value.is_nan() {
                    "OL".into()
                } else {
                    let (m, p) = format::scale(d.value);
                    format!("{} {p}{}", format::sig(m, dmm_digits), s.dmm.function.unit())
                }
            }),
            dmm_function: s.dmm.function.label(),
            dmm_tag: s.dmm.function.tag(),
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

#[derive(Clone)]
struct AppState {
    store: Shared,
    dmm_digits: usize,
}

/// Running server; dropping it shuts the server down.
pub struct OverlayServer {
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub error: Option<String>,
}

impl OverlayServer {
    pub fn start(store: Shared, addr: SocketAddr, dmm_digits: usize) -> Self {
        let (tx, rx) = oneshot::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let state = AppState { store, dmm_digits };
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
                    let listener = match tokio::net::TcpListener::bind(addr).await {
                        Ok(l) => {
                            let _ = ready_tx.send(Ok(()));
                            l
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(format!("Port {}: {e}", addr.port())));
                            return;
                        }
                    };
                    let app = router(state);
                    let _ = axum::serve(listener, app)
                        .with_graceful_shutdown(async {
                            let _ = rx.await;
                        })
                        .await;
                });
            })
            .expect("spawn overlay server");
        let error = match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e),
            Err(_) => Some("Server startet nicht".into()),
        };
        Self { shutdown: Some(tx), thread: Some(thread), error }
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
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route("/overlay", get(|| async { Html(OVERLAY_HTML) }))
        .route("/api/live", get(live))
        .route("/events", get(events))
        .with_state(state)
}

async fn live(State(st): State<AppState>) -> impl IntoResponse {
    let snap = {
        let s = st.store.lock().unwrap();
        Snapshot::build(&s, st.dmm_digits, true)
    };
    ([("Access-Control-Allow-Origin", "*")], Json(snap))
}

async fn events(State(st): State<AppState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = async_stream(st);
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(5)))
}

fn async_stream(st: AppState) -> impl Stream<Item = Result<Event, Infallible>> {
    futures_util::stream::unfold((st, (u64::MAX, u64::MAX), 0u32), |(st, mut last_seq, mut tick)| async move {
        let mut interval = tokio::time::interval(Duration::from_millis(16));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            tick = tick.wrapping_add(1);
            let with_spark = tick % 12 == 0; // ~5 Hz
            let json = {
                let s = st.store.lock().unwrap();
                let seq = (s.power.seq, s.dmm.seq);
                // send on new data, plus a spark refresh even when idle so
                // the "disconnected" state shows up
                if seq == last_seq && !with_spark {
                    continue;
                }
                last_seq = seq;
                serde_json::to_string(&Snapshot::build(&s, st.dmm_digits, with_spark)).unwrap_or_default()
            };
            return Some((Ok(Event::default().data(json)), (st, last_seq, tick)));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
