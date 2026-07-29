mod app;
mod config;
mod manifest;
mod pages;
mod process;
mod sidebar;
mod tab;
mod theme;

use iced::window;

use app::ManagerApp;
use config::ManagerConfig;

fn main() -> iced::Result {
    let config = ManagerConfig::load();
    let w = config.width.map(|x| x as f32);
    let h = config.height.map(|x| x as f32);

    iced::application(
        move || ManagerApp::new(config.clone()),
        ManagerApp::update,
        ManagerApp::view,
    )
    .title(ManagerApp::title)
    .theme(ManagerApp::theme)
    .subscription(ManagerApp::subscription)
    .window(window::Settings {
        size: match (w, h) {
            (Some(w), Some(h)) => iced::Size::new(w, h),
            _ => iced::Size::new(860.0, 620.0),
        },
        ..Default::default()
    })
    .run()
}
