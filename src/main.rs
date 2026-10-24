//! Narration Studio: multi-voice audiobook narration with per-segment review.

mod acx;
mod app;
mod audio;
mod claude_code;
mod config;
mod costs;
mod db;
mod export;
mod hash;
mod lang;
mod llm;
mod model;
mod player;
mod retail;
mod segment;
mod tags;
mod theme;
mod tts;
mod voices;
mod worker;

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Narration Studio")
            .with_app_id("narration-studio")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Narration Studio",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)?))),
    )
}
