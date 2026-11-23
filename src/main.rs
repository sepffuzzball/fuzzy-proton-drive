mod drive;
mod storage;
mod sync;
mod ui;
mod worker;

const APP_ICON: &[u8] = include_bytes!("../packaging/io.github.fuzzy_proton_drive.png");

fn main() -> eframe::Result<()> {
    let icon = eframe::icon_data::from_png_bytes(APP_ICON).ok();
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_app_id("io.github.fuzzy_proton_drive")
        .with_inner_size([1080.0, 688.0])
        .with_min_inner_size([740.0, 420.0]);
    if let Some(icon) = &icon {
        viewport = viewport.with_icon(icon.clone());
    }
    eframe::run_native(
        "Fuzzy Proton Drive",
        eframe::NativeOptions {
            viewport,
            ..Default::default()
        },
        Box::new(move |cc| Ok(Box::new(ui::App::new(&cc.egui_ctx, icon)))),
    )
}
