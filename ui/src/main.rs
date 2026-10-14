//! Depth desktop app (egui build).
use t1_ui as ui;

fn main() -> eframe::Result<()> {
    #[cfg(feature = "hotpatch")]
    dioxus_devtools::connect_subsecond();
    let opts = eframe::NativeOptions {
        // no native title bar: the app draws its own (window buttons, drag, resize edges)
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1680.0, 1020.0]).with_min_inner_size([1100.0, 700.0])
            .with_title("Depth").with_decorations(false).with_resizable(true)
            // screenshot runs never take focus from the user's windows
            .with_active(std::env::var_os("T1_SHOT").is_none())
            // the app's own icon: eframe would otherwise put the egui logo in the Dock
            .with_icon(std::sync::Arc::new(eframe::icon_data::from_png_bytes(include_bytes!("../../assets/appicon/icon-1024.png")).unwrap_or_default())),
        ..Default::default()
    };
    eframe::run_native("Depth", opts, Box::new(|cc| Ok(Box::new(ui::App::new(cc)))))
}
