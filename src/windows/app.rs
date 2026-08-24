use super::{
    capture::CaptureSession,
    monitor::{DisplayInfo, enumerate_displays},
};
use crate::{
    config::SenderConfig,
    server::{LocalServer, StreamState},
};
use eframe::egui;
use local_ip_address::local_ip;
use std::{
    net::{IpAddr, Ipv4Addr},
    sync::Arc,
    time::Duration,
};

struct RunningSender {
    server: LocalServer,
    capture: CaptureSession,
    state: Arc<StreamState>,
    url: String,
}

pub struct SenderApp {
    config: SenderConfig,
    displays: Vec<DisplayInfo>,
    running: Option<RunningSender>,
    message: Option<(bool, String)>,
}

impl SenderApp {
    fn new(context: &eframe::CreationContext<'_>) -> Self {
        context.egui_ctx.set_pixels_per_point(1.15);
        let mut app = Self {
            config: SenderConfig::default(),
            displays: Vec::new(),
            running: None,
            message: None,
        };
        app.refresh_displays();
        app
    }

    fn refresh_displays(&mut self) {
        match enumerate_displays() {
            Ok(displays) if !displays.is_empty() => {
                self.displays = displays;
                if !self
                    .displays
                    .iter()
                    .any(|display| display.index == self.config.monitor_index)
                {
                    self.config.monitor_index = self.displays[0].index;
                }
                self.message = None;
            }
            Ok(_) => {
                self.message = Some((
                    false,
                    "Windows meldet keinen aktiven Bildschirm.".to_owned(),
                ))
            }
            Err(error) => self.message = Some((false, error.to_string())),
        }
    }

    fn start(&mut self) {
        if let Err(error) = self.config.validate() {
            self.message = Some((false, error));
            return;
        }
        if !self
            .displays
            .iter()
            .any(|display| display.index == self.config.monitor_index)
        {
            self.message = Some((
                false,
                "Der ausgewählte Bildschirm ist nicht mehr verfügbar.".to_owned(),
            ));
            return;
        }

        let state = StreamState::new(self.config.pin.clone());
        let server = match LocalServer::start(self.config.socket_addr(), Arc::clone(&state)) {
            Ok(server) => server,
            Err(error) => {
                self.message = Some((
                    false,
                    format!("Webserver konnte nicht gestartet werden: {error}"),
                ));
                return;
            }
        };
        let capture = match CaptureSession::start(
            self.config.monitor_index,
            self.config.fps,
            self.config.bitrate_kbps,
            self.config.capture_cursor,
            Arc::clone(&state),
        ) {
            Ok(capture) => capture,
            Err(error) => {
                server.stop();
                self.message = Some((false, error.to_string()));
                return;
            }
        };

        let ip = local_ip().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let url = self.config.browser_url(ip);
        self.running = Some(RunningSender {
            server,
            capture,
            state,
            url: url.clone(),
        });
        self.message = Some((true, format!("Stream läuft unter {url}")));
    }

    fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            let capture_result = running.capture.stop();
            running.server.stop();
            self.message = Some(match capture_result {
                Ok(()) => (true, "Stream wurde beendet.".to_owned()),
                Err(error) => (false, format!("Stream beendet; Aufnahmefehler: {error}")),
            });
        }
    }
}

impl eframe::App for SenderApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        context.request_repaint_after(Duration::from_millis(250));

        if let Some(running) = &mut self.running
            && let Some(result) = running.capture.wait_if_finished()
        {
            let message = match result {
                Ok(()) => "Die Bildschirmaufnahme wurde von Windows beendet.".to_owned(),
                Err(error) => format!("Bildschirmaufnahme abgebrochen: {error}"),
            };
            if let Some(running) = self.running.take() {
                running.server.stop();
            }
            self.message = Some((false, message));
        }

        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Tesla Screen Sender");
            ui.label("Eigenständige Monitorübertragung für den Tesla-Browser im lokalen Netzwerk");
            ui.add_space(12.0);

            ui.add_enabled_ui(self.running.is_none(), |ui| {
                ui.horizontal(|ui| {
                    ui.label("Bildschirm");
                    let selected = self.displays.iter()
                        .find(|display| display.index == self.config.monitor_index)
                        .map(DisplayInfo::label)
                        .unwrap_or_else(|| "Kein Bildschirm".to_owned());
                    egui::ComboBox::from_id_salt("monitor")
                        .selected_text(selected)
                        .width(470.0)
                        .show_ui(ui, |ui| {
                            for display in &self.displays {
                                ui.selectable_value(&mut self.config.monitor_index, display.index, display.label());
                            }
                        });
                    if ui.button("Neu laden").clicked() {
                        self.refresh_displays();
                    }
                });

                egui::Grid::new("settings").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
                    ui.label("Webserver-Port");
                    ui.add(egui::DragValue::new(&mut self.config.port).range(1..=u16::MAX));
                    ui.end_row();

                    ui.label("Bildrate");
                    ui.add(egui::Slider::new(&mut self.config.fps, 1..=60).suffix(" FPS"));
                    ui.end_row();

                    ui.label("Bitrate");
                    ui.add(egui::Slider::new(&mut self.config.bitrate_kbps, 500..=50_000).suffix(" kbit/s"));
                    ui.end_row();

                    ui.label("Anmelde-PIN");
                    ui.add(egui::TextEdit::singleline(&mut self.config.pin).password(true).desired_width(180.0));
                    ui.end_row();

                    ui.label("Mauszeiger übertragen");
                    ui.checkbox(&mut self.config.capture_cursor, "anzeigen");
                    ui.end_row();
                });
            });

            ui.add_space(12.0);
            if self.running.is_some() {
                if ui.add_sized([150.0, 36.0], egui::Button::new("Stream stoppen")).clicked() {
                    self.stop();
                }
            } else if ui.add_sized([150.0, 36.0], egui::Button::new("Stream starten")).clicked() {
                self.start();
            }

            if let Some(running) = &self.running {
                ui.add_space(16.0);
                ui.separator();
                ui.heading("Tesla-Verbindung");
                ui.horizontal(|ui| {
                    ui.monospace(&running.url);
                    if ui.button("Adresse kopieren").clicked() {
                        context.copy_text(running.url.clone());
                    }
                });
                ui.label(format!("PIN: {}", self.config.pin));
                let snapshot = running.state.snapshot();
                ui.label(format!(
                    "{} Browser verbunden · {} Frames · {} × {}",
                    snapshot.clients, snapshot.frames, snapshot.width, snapshot.height
                ));
                if snapshot.jpeg_clients > 0 {
                    ui.small(format!(
                        "{} Browser im HTTP-Kompatibilitätsmodus (JPEG)",
                        snapshot.jpeg_clients
                    ));
                }
                ui.small("Tesla, Smartphone und dieser PC müssen mit demselben Router/WLAN verbunden sein.");
            }

            if let Some((success, message)) = &self.message {
                ui.add_space(12.0);
                ui.colored_label(
                    if *success { egui::Color32::LIGHT_GREEN } else { egui::Color32::LIGHT_RED },
                    message,
                );
            }
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop();
    }
}

pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 540.0])
            .with_min_inner_size([680.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Tesla Screen Sender",
        options,
        Box::new(|context| Ok(Box::new(SenderApp::new(context)))),
    )
}
