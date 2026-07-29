use std::path::PathBuf;
use std::time::Duration;

use iced::widget::{container, Row};
use iced::{Element, Length, Subscription, Task};

use crate::config::ManagerConfig;
use crate::manifest::AppManifest;
use crate::pages::geek_familiar::GeekFamiliarState;
use crate::process::{AppStatus, ProcessRegistry};
use crate::sidebar;
use crate::tab::Tab;
use crate::theme;

pub struct ManagerApp {
    pub active_tab: Tab,
    pub manifest: AppManifest,
    pub workspace_root: PathBuf,
    pub process_registry: ProcessRegistry,
    pub app_status: AppStatus,
    pub gf_state: GeekFamiliarState,
    pub config: ManagerConfig,
    /// Current window size (tracked via resize events). None until first event.
    pub window_size: Option<(u32, u32)>,
    /// Whether we have a pending save (debounce).
    pub dirty_config: bool,
}

#[derive(Debug, Clone)]
pub enum Message {
    TabSelected(Tab),
    LaunchApp,
    StopApp,
    RefreshStatus,
    GfSkinSelected(String),
    GfAuraCheck,
    GfAuraCheckResult(bool),
    GfLaunchToggled,
    GfConfigTextChanged(String),
    GfConfigSaved,
    /// Window was resized (from `window::resize_events()` subscription).
    WindowResized(u32, u32),
    /// Periodic tick for process monitoring + saving config if dirty.
    Tick,
}

const TICK_SECS: u64 = 2;

/// 2-second tick stream for process monitoring + config save.
fn tick_stream(
    _dummy: &String,
) -> std::pin::Pin<Box<dyn iced::futures::Stream<Item = Message> + Send>> {
    Box::pin(iced::stream::channel::<Message>(
        4,
        move |mut sender: iced::futures::channel::mpsc::Sender<Message>| async move {
            loop {
                std::thread::sleep(Duration::from_secs(TICK_SECS));
                let _ = sender.try_send(Message::Tick);
            }
        },
    ))
}

impl ManagerApp {
    pub fn new(config: ManagerConfig) -> (Self, Task<Message>) {
        let manifest = crate::manifest::load_manifest("geek-familiar")
            .expect("geek-familiar manifest not found in MANIFESTS namespace");
        let workspace_root = crate::manifest::detect_workspace_root();
        let process_registry = ProcessRegistry::new();
        let gf_state = GeekFamiliarState::new();

        let app = Self {
            active_tab: Tab::Home,
            manifest,
            workspace_root,
            process_registry,
            app_status: AppStatus::Stopped,
            gf_state,
            config,
            window_size: None,
            dirty_config: false,
        };

        (app, Task::none())
    }

    fn binary_path(&self) -> PathBuf {
        self.workspace_root.join(&self.manifest.exec.binary_relative)
    }

    pub fn title(&self) -> String { "Desk Pilot Box".into() }

    pub fn theme(&self) -> iced::Theme { theme::manager_theme() }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::TabSelected(tab) => { self.active_tab = tab; Task::none() }

            Message::LaunchApp | Message::GfLaunchToggled => {
                let bin = self.binary_path();
                let name = self.manifest.name.clone();
                if self.process_registry.is_registered(&name) {
                    self.app_status = self.process_registry.stop(&name);
                }
                self.app_status = self.process_registry.launch(&name, &bin.to_string_lossy());
                Task::none()
            }

            Message::StopApp => {
                let name = self.manifest.name.clone();
                self.app_status = self.process_registry.stop(&name);
                Task::none()
            }

            Message::RefreshStatus => {
                let exited = self.process_registry.poll_all();
                if exited.contains(&self.manifest.name) {
                    self.app_status = AppStatus::Stopped;
                }
                Task::none()
            }

            Message::GfSkinSelected(skin) => {
                self.gf_state.selected_skin = skin;
                Task::none()
            }

            Message::GfAuraCheck => {
                let addr = "127.0.0.1:9091".to_string();
                Task::perform(
                    async move {
                        use std::io::{Read, Write};
                        use std::net::TcpStream;
                        let Ok(mut stream) = TcpStream::connect_timeout(
                            &addr.parse().unwrap(),
                            std::time::Duration::from_secs(2),
                        ) else { return false; };
                        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                        if stream.write_all(b"GET /health HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n").is_err() {
                            return false;
                        }
                        let mut buf = [0u8; 256];
                        let Ok(n) = stream.read(&mut buf) else { return false; };
                        String::from_utf8_lossy(&buf[..n]).contains("200")
                    },
                    Message::GfAuraCheckResult,
                )
            }

            Message::GfAuraCheckResult(ok) => {
                self.gf_state.aura_connected = Some(ok);
                Task::none()
            }

            Message::GfConfigTextChanged(text) => {
                self.gf_state.config_text = text;
                self.gf_state.config_saved = false;
                Task::none()
            }

            Message::GfConfigSaved => {
                let loader = fs::loader!();
                let _ = loader.write_str("CONF::familiar.yaml", &self.gf_state.config_text);
                self.gf_state.config_saved = true;
                Task::none()
            }

            Message::WindowResized(w, h) => {
                // Only persist if the size actually changed
                let changed = match self.window_size {
                    Some((pw, ph)) => pw != w || ph != h,
                    None => true,
                };
                self.window_size = Some((w, h));
                if changed {
                    self.dirty_config = true;
                }
                Task::none()
            }

            Message::Tick => {
                // Poll process status
                let exited = self.process_registry.poll_all();
                if exited.contains(&self.manifest.name) {
                    self.app_status = AppStatus::Stopped;
                }
                // Save config if dirty (debounced by tick interval)
                if self.dirty_config {
                    if let Some((w, h)) = self.window_size {
                        self.config.width = Some(w);
                        self.config.height = Some(h);
                    }
                    self.config.save();
                    self.dirty_config = false;
                }
                Task::none()
            }
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        let side = sidebar::sidebar(&self.active_tab);
        let page: Element<'_, Message> = match &self.active_tab {
            Tab::Home => crate::pages::home::view(&self.manifest, &self.app_status),
            Tab::AppStore => crate::pages::app_store::view(),
            Tab::GeekFamiliar => crate::pages::geek_familiar::view(&self.gf_state),
        };
        container(Row::with_children(vec![side.into(), page.into()])).width(Length::Fill).height(Length::Fill).into()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            Subscription::run_with("manager-tick".to_string(), tick_stream),
            iced::window::resize_events().map(|(_id, size)| {
                Message::WindowResized(size.width.round() as u32, size.height.round() as u32)
            }),
        ])
    }
}
