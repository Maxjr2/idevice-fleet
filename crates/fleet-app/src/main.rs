//! iDevice Fleet desktop app.

mod theme;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;

use fleet_core::{Config, Fleet, Limits};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// The value after `--name`, or from `--name=value`.
fn arg(name: &str) -> Option<String> {
    let flag = format!("--{name}");
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == flag {
            return args.next();
        }
        if let Some(v) = a.strip_prefix(&format!("{flag}=")) {
            return Some(v.to_string());
        }
    }
    None
}

fn has_flag(name: &str) -> bool {
    std::env::args().skip(1).any(|a| a == format!("--{name}"))
}

fn data_dir() -> PathBuf {
    if has_flag("demo") {
        // Demo data lives apart from the real database and is rebuilt every run.
        let d = std::env::temp_dir().join("idevice-fleet-demo");
        let _ = std::fs::remove_dir_all(&d);
        return d;
    }
    if let Some(p) = arg("data") {
        return PathBuf::from(p);
    }
    if let Some(p) = std::env::var_os("IDEVICE_FLEET_DATA") {
        return PathBuf::from(p);
    }
    directories::ProjectDirs::from("", "", "idevice-fleet")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("idevice-fleet-data"))
}

fn init_logging(dir: &std::path::Path) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,idevice=warn,idevice_fleet=info,fleet_core=debug,job=info"));
    let registry = tracing_subscriber::registry().with(filter).with(fmt::layer().with_writer(std::io::stderr));
    match std::fs::create_dir_all(dir) {
        Ok(()) => {
            let (writer, guard) = tracing_appender::non_blocking(tracing_appender::rolling::daily(dir, "app.log"));
            registry.with(fmt::layer().with_ansi(false).with_writer(writer)).init();
            Some(guard)
        }
        Err(_) => {
            registry.init();
            None
        }
    }
}

fn main() -> eframe::Result {
    let data = data_dir();
    let _log_guard = init_logging(&data.join("logs"));

    // A panic anywhere is logged before the default handler runs.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        default_hook(info);
    }));

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().thread_name("fleet-worker").build().expect("tokio runtime");

    let started = {
        let _guard = runtime.enter();
        Fleet::start(Config { data_dir: data.clone(), limits: Limits::default(), demo: has_flag("demo") })
    };

    let (w, h) = arg("size").and_then(|s| s.split_once('x').map(|(w, h)| (w.parse().unwrap_or(1180.0), h.parse().unwrap_or(760.0)))).unwrap_or((1180.0, 760.0));
    let ui_options = ui::Options {
        screenshot: arg("screenshot").map(PathBuf::from),
        tab: arg("tab"),
        lookup: arg("lookup"),
        theme: arg("theme"),
        select_job: has_flag("demo"),
    };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([w, h])
            .with_min_inner_size([720.0, 480.0])
            .with_app_id("idevice-fleet"),
        ..Default::default()
    };

    match started {
        Ok(fleet) => {
            let fleet = Arc::new(fleet);
            tracing::info!("started, data folder {}", data.display());
            let ui_fleet = fleet.clone();
            let result = eframe::run_native("iDevice Fleet", options, Box::new(move |cc| Ok(Box::new(ui::FleetApp::new(cc, ui_fleet, ui_options)))));
            // Window closed: stop jobs cleanly before the runtime goes away.
            runtime.block_on(fleet.shutdown());
            result
        }
        Err(e) => {
            tracing::error!("could not start: {e}");
            let message = e.message.clone();
            eframe::run_native("iDevice Fleet", options, Box::new(move |_| Ok(Box::new(ui::StartupError(message)))))
        }
    }
}
