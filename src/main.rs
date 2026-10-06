#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod devices;
mod export;
mod format;
mod model;
mod overlay;

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--probe-spm") {
        std::process::exit(probe_spm(&args[1..]));
    }
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("PowerMeter")
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native("PowerMeter", options, Box::new(|cc| Ok(Box::new(app::PowerMeterApp::new(cc)))))
}

/// `powermeter --probe-spm <port> [baud]`: reads an OWON SPM once (read
/// only) and prints what it answers, without starting the window. `sim`
/// as port talks to the simulated SPM.
fn probe_spm(args: &[String]) -> i32 {
    let Some(port) = args.first() else {
        eprintln!("Aufruf: powermeter --probe-spm <port> [baud]   z. B. /dev/cu.usbserial-1410 115200");
        return 2;
    };
    let baud = match args.get(1).map(|b| b.parse::<u32>()) {
        None => 115_200,
        Some(Ok(b)) => b,
        Some(Err(_)) => {
            eprintln!("Ungültige Baudrate: {}", args[1]);
            return 2;
        }
    };
    println!("OWON SPM an {port}, {baud} Baud (nur lesen)\n");
    let mut out = std::io::stdout();
    let result = if port == "sim" {
        let dev = devices::sim::SpmDevice::shared();
        devices::owon_spm::probe(baud, move |_| Ok(devices::sim::FakeSpm::new(dev.clone())), &mut out)
    } else {
        devices::owon_spm::probe(baud, |b| devices::owon_spm::open_serial(port, b), &mut out)
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Fehler: {e}");
            1
        }
    }
}
