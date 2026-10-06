//! CSV export of the recorded session.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use chrono::{DateTime, Local, TimeDelta};

use crate::model::{DmmSample, PowerEvent, PowerSample, Shared};

pub fn export_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join("Documents").join("PowerMeter")
}

/// Writes `netzteil.csv`, `multimeter.csv` and `ereignisse.csv` into a new
/// folder and returns its path.
pub fn export_session(store: &Shared) -> std::io::Result<PathBuf> {
    let (power, dmm, events, unit, start): (Vec<PowerSample>, Vec<DmmSample>, Vec<PowerEvent>, &str, DateTime<Local>) = {
        let s = store.lock().unwrap();
        let start = Local::now() - TimeDelta::from_std(s.t0().elapsed()).unwrap_or_default();
        (
            s.power.samples.iter().copied().collect(),
            s.dmm.samples.iter().copied().collect(),
            s.power.analyzer.events.iter().cloned().collect(),
            s.dmm.function.unit(),
            start,
        )
    };
    let dir = export_dir().join(format!("Sitzung_{}", Local::now().format("%Y-%m-%d_%H-%M-%S")));
    fs::create_dir_all(&dir)?;
    let stamp =
        |t: f64| (start + TimeDelta::microseconds((t * 1e6) as i64)).format("%Y-%m-%d %H:%M:%S%.3f").to_string();

    let mut w = BufWriter::new(File::create(dir.join("netzteil.csv"))?);
    writeln!(w, "zeit_s;uhrzeit;spannung_V;strom_A;leistung_W")?;
    for s in &power {
        writeln!(w, "{:.4};{};{:.5};{:.6};{:.5}", s.t, stamp(s.t), s.v, s.i, s.p())?;
    }
    w.flush()?;

    let mut w = BufWriter::new(File::create(dir.join("multimeter.csv"))?);
    writeln!(w, "zeit_s;uhrzeit;wert_{unit}")?;
    for s in &dmm {
        if s.value.is_nan() {
            writeln!(w, "{:.4};{};OL", s.t, stamp(s.t))?;
        } else {
            writeln!(w, "{:.4};{};{:e}", s.t, stamp(s.t), s.value)?;
        }
    }
    w.flush()?;

    let mut w = BufWriter::new(File::create(dir.join("ereignisse.csv"))?);
    writeln!(w, "start_s;uhrzeit;ende_s;dauer_ms;art;min_spannung_V;max_strom_A")?;
    for e in &events {
        let end = e.t_end.unwrap_or(e.t_start);
        writeln!(
            w,
            "{:.4};{};{:.4};{:.1};{};{:.4};{:.5}",
            e.t_start,
            stamp(e.t_start),
            end,
            (end - e.t_start) * 1000.0,
            e.kind.label(),
            e.v_min,
            e.i_max
        )?;
    }
    w.flush()?;
    Ok(dir)
}
