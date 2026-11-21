#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod designs;
mod devices;
mod export;
mod format;
mod model;
mod overlay;

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--probe-spm") {
        attach_console();
        std::process::exit(probe_spm(&args[1..]));
    }
    if args.first().map(String::as_str) == Some("--probe-xdm") {
        attach_console();
        std::process::exit(probe_xdm(&args[1..]));
    }
    if args.first().map(String::as_str) == Some("--serve-only") {
        attach_console();
        std::process::exit(serve_only(&args[1..]));
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

/// The release build on Windows is a GUI program without a console, so
/// `--probe-spm` would print into the void: borrow the console of the
/// PowerShell / cmd window it was started from.
#[cfg(target_os = "windows")]
fn attach_console() {
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
    }
    // SAFETY: plain Win32 call without pointers; failing (no parent
    // console, or output already redirected) is harmless.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(target_os = "windows"))]
fn attach_console() {}

/// `powermeter --probe-spm <port|auto|sim> [baud]`: reads an OWON SPM/SPE
/// once (read only) and prints what it answers, without starting the
/// window. `auto` takes the first USB-serial adapter, `sim` talks to the
/// simulated SPM.
fn probe_spm(args: &[String]) -> i32 {
    let Some(port) = args.first() else {
        eprintln!("Aufruf: powermeter --probe-spm <port|auto> [baud]   z. B. COM3, /dev/cu.usbserial-1410, auto");
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
    let mut out = std::io::stdout();
    let result = devices::owon_spm::probe_port(port, baud, &mut out);
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Fehler: {e}");
            1
        }
    }
}

/// `powermeter --probe-xdm <port> [baud]`: asks an OWON XDM a fixed list of
/// questions and prints the raw answers (see `devices::owon::probe`).
fn probe_xdm(args: &[String]) -> i32 {
    let Some(port) = args.first() else {
        eprintln!("Aufruf: powermeter --probe-xdm <port> [baud]   z. B. COM4, /dev/cu.usbserial-1420");
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
    match devices::owon::probe_port(port, baud, &mut std::io::stdout()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Fehler: {e}");
            1
        }
    }
}

/// `powermeter --serve-only [port]`: only the overlay server, fed by the
/// power and multimeter simulators. For trying out the Overlay-Studio
/// without a display or hardware (and for screenshots). Not in the usage
/// text; runs until it is stopped.
fn serve_only(args: &[String]) -> i32 {
    use std::net::{Ipv4Addr, SocketAddr};
    use std::sync::{Arc, Mutex, atomic::AtomicBool, mpsc};
    let port = match args.first().map(|p| p.parse::<u16>()) {
        None => 8765,
        Some(Ok(p)) => p,
        Some(Err(_)) => {
            eprintln!("Ungültiger Port: {}", args[0]);
            return 2;
        }
    };
    let store = model::Store::shared();
    let designs = designs::DesignStore::open_default();
    if let Some(note) = &designs.note {
        eprintln!("{note}");
    }
    let designs = Arc::new(Mutex::new(designs));
    let ctx = eframe::egui::Context::default();
    let stop = Arc::new(AtomicBool::new(false));
    let (_dmm_tx, dmm_rx) = mpsc::channel();
    let (s, c, st) = (store.clone(), ctx.clone(), stop.clone());
    std::thread::spawn(move || devices::sim::run_power(s, c, 100.0, st));
    let (s, st) = (store.clone(), stop.clone());
    std::thread::spawn(move || devices::sim::run_dmm(s, ctx, dmm_rx, st));
    let server = overlay::OverlayServer::start(store, designs, SocketAddr::from((Ipv4Addr::LOCALHOST, port)), 5);
    if let Some(e) = &server.error {
        eprintln!("{e}");
        return 1;
    }
    println!("Overlay-Studio: http://127.0.0.1:{}/studio", server.port.unwrap_or(port));
    loop {
        std::thread::park();
    }
}
