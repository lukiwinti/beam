use super::{
    acme::{
        AcmeRequest, CertificateRenewer, PROVIDERS, PreparedCertificate, prepare_certificate,
        provider,
    },
    audio::AudioSession,
    capture::CaptureSession,
    input::InputSession,
    monitor::{DisplayInfo, enumerate_displays},
    secrets::SecretStore,
};
use crate::{
    config::SenderConfig,
    server::{LocalServer, StreamState, TlsIdentity},
};
use eframe::egui;
use local_ip_address::local_ip;
use std::{
    net::{IpAddr, Ipv4Addr},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

struct RunningSender {
    server: LocalServer,
    capture: CaptureSession,
    audio: Option<AudioSession>,
    input: Option<InputSession>,
    certificate_renewer: Option<CertificateRenewer>,
    state: Arc<StreamState>,
    url: String,
}

pub struct SenderApp {
    config: SenderConfig,
    secrets: SecretStore,
    displays: Vec<DisplayInfo>,
    running: Option<RunningSender>,
    pending_start: Option<mpsc::Receiver<anyhow::Result<PreparedCertificate>>>,
    message: Option<(bool, String)>,
    show_settings: bool,
    fps_sample_at: Instant,
    fps_sample_frames: u64,
    measured_fps: f64,
}

impl SenderApp {
    fn new(context: &eframe::CreationContext<'_>) -> Self {
        context.egui_ctx.set_pixels_per_point(1.15);
        let (config, load_error) = match SenderConfig::load() {
            Ok(config) => (config, None),
            Err(error) => (SenderConfig::default(), Some(error.to_string())),
        };
        let (secrets, secret_error) = match SecretStore::load() {
            Ok(secrets) => (secrets, None),
            Err(error) => (SecretStore::default(), Some(error.to_string())),
        };
        let mut app = Self {
            config,
            secrets,
            displays: Vec::new(),
            running: None,
            pending_start: None,
            message: None,
            show_settings: false,
            fps_sample_at: Instant::now(),
            fps_sample_frames: 0,
            measured_fps: 0.0,
        };
        app.refresh_displays();
        if let Some(error) = load_error {
            app.message = Some((
                false,
                format!("Einstellungen konnten nicht geladen werden: {error}"),
            ));
        } else if let Some(error) = secret_error {
            app.message = Some((
                false,
                format!("Gespeicherte DNS-Zugangsdaten konnten nicht geladen werden: {error}"),
            ));
        }
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
        if let Err(error) = self.save_settings() {
            self.message = Some((
                false,
                format!("Einstellungen konnten nicht gespeichert werden: {error}"),
            ));
            return;
        }

        if self.config.https_enabled {
            let request = AcmeRequest {
                domain: self.config.https_domain.trim().to_owned(),
                email: self.config.acme_email.trim().to_owned(),
                provider: self.config.dns_provider.clone(),
                credentials: self.secrets.credentials(&self.config.dns_provider),
            };
            if let Err(error) = request.validate_credentials() {
                self.message = Some((false, error.to_string()));
                return;
            }
            let (result_tx, result_rx) = mpsc::sync_channel(1);
            if let Err(error) = std::thread::Builder::new()
                .name("tesla-screen-acme-prepare".to_owned())
                .spawn(move || {
                    let _ = result_tx.send(prepare_certificate(request));
                })
            {
                self.message = Some((
                    false,
                    format!("Zertifikatsprüfung konnte nicht gestartet werden: {error}"),
                ));
                return;
            }
            self.pending_start = Some(result_rx);
            self.message = Some((
                true,
                "Let's-Encrypt-Zertifikat wird im Hintergrund geprüft oder ausgestellt …"
                    .to_owned(),
            ));
        } else {
            self.start_ready(None);
        }
    }

    fn start_ready(&mut self, certificate: Option<PreparedCertificate>) {
        let state = StreamState::new(self.config.pin.clone());
        let input = if self.config.control_enabled {
            match InputSession::start(Arc::clone(&state), self.config.monitor_index) {
                Ok(input) => Some(input),
                Err(error) => {
                    self.message = Some((false, error.to_string()));
                    return;
                }
            }
        } else {
            None
        };
        let tls = certificate.as_ref().map(|certificate| TlsIdentity {
            cert_path: certificate.cert_path.clone(),
            key_path: certificate.key_path.clone(),
        });
        let server = match LocalServer::start(self.config.socket_addr(), Arc::clone(&state), tls) {
            Ok(server) => server,
            Err(error) => {
                if let Some(input) = input {
                    input.stop();
                }
                self.message = Some((
                    false,
                    format!("Webserver konnte nicht gestartet werden: {error}"),
                ));
                return;
            }
        };
        let started_at = Instant::now();
        let capture = match CaptureSession::start(
            self.config.monitor_index,
            self.config.fps,
            self.config.bitrate_kbps,
            self.config.capture_cursor,
            Arc::clone(&state),
            started_at,
        ) {
            Ok(capture) => capture,
            Err(error) => {
                if let Some(input) = input {
                    input.stop();
                }
                server.stop();
                self.message = Some((false, error.to_string()));
                return;
            }
        };
        let audio = if self.config.audio_enabled {
            match AudioSession::start(Arc::clone(&state), started_at) {
                Ok(audio) => Some(audio),
                Err(error) => {
                    if let Some(input) = input {
                        input.stop();
                    }
                    let _ = capture.stop();
                    server.stop();
                    self.message = Some((false, error.to_string()));
                    return;
                }
            }
        } else {
            None
        };

        let ip = local_ip().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let url = self.config.browser_url(ip);
        let certificate_renewer = certificate.and_then(|certificate| {
            server
                .tls_config()
                .map(|tls| certificate.start_renewer(tls))
        });
        self.running = Some(RunningSender {
            server,
            capture,
            audio,
            input,
            certificate_renewer,
            state,
            url: url.clone(),
        });
        self.fps_sample_at = Instant::now();
        self.fps_sample_frames = 0;
        self.measured_fps = 0.0;
        self.message = Some((true, format!("Stream läuft unter {url}")));
    }

    fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            if let Some(renewer) = running.certificate_renewer {
                renewer.stop();
            }
            let audio_result = running.audio.map(AudioSession::stop).unwrap_or(Ok(()));
            if let Some(input) = running.input {
                input.stop();
            }
            let capture_result = running.capture.stop();
            running.server.stop();
            self.message = Some(match (capture_result, audio_result) {
                (Ok(()), Ok(())) => (true, "Stream wurde beendet.".to_owned()),
                (Err(error), _) => (false, format!("Stream beendet; Aufnahmefehler: {error}")),
                (_, Err(error)) => (false, format!("Stream beendet; Audiofehler: {error}")),
            });
        }
    }

    fn save_settings(&self) -> anyhow::Result<std::path::PathBuf> {
        let path = self.config.save()?;
        self.secrets.save()?;
        Ok(path)
    }

    fn poll_pending_start(&mut self) {
        let result = self
            .pending_start
            .as_ref()
            .and_then(|receiver| match receiver.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(anyhow::anyhow!(
                    "Zertifikatsprüfung wurde unerwartet beendet"
                ))),
            });
        if let Some(result) = result {
            self.pending_start = None;
            match result {
                Ok(certificate) => self.start_ready(Some(certificate)),
                Err(error) => {
                    self.message = Some((false, error.to_string()));
                }
            }
        }
    }

    fn settings_window(&mut self, context: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let mut open = self.show_settings;
        egui::Window::new("Einstellungen")
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .default_width(620.0)
            .vscroll(true)
            .show(context, |ui| {
                ui.add_enabled_ui(self.running.is_none() && self.pending_start.is_none(), |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Bildschirm");
                        let selected = self
                            .displays
                            .iter()
                            .find(|display| display.index == self.config.monitor_index)
                            .map(DisplayInfo::label)
                            .unwrap_or_else(|| "Kein Bildschirm".to_owned());
                        egui::ComboBox::from_id_salt("settings-monitor")
                            .selected_text(selected)
                            .width(390.0)
                            .show_ui(ui, |ui| {
                                for display in &self.displays {
                                    ui.selectable_value(
                                        &mut self.config.monitor_index,
                                        display.index,
                                        display.label(),
                                    );
                                }
                            });
                        if ui.button("Neu laden").clicked() {
                            self.refresh_displays();
                        }
                    });
                    ui.add_space(8.0);
                    egui::Grid::new("persistent-settings")
                        .num_columns(2)
                        .spacing([16.0, 10.0])
                        .show(ui, |ui| {
                            ui.label("Webserver-Port");
                            ui.add(egui::DragValue::new(&mut self.config.port).range(1..=u16::MAX));
                            ui.end_row();

                            ui.label("Bildrate");
                            ui.add(egui::Slider::new(&mut self.config.fps, 1..=60).suffix(" FPS"));
                            ui.end_row();

                            ui.label("Bitrate");
                            ui.add(
                                egui::Slider::new(&mut self.config.bitrate_kbps, 500..=50_000)
                                    .suffix(" kbit/s"),
                            );
                            ui.end_row();

                            ui.label("Anmelde-PIN");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.config.pin)
                                    .password(true)
                                    .desired_width(180.0),
                            );
                            ui.end_row();

                            ui.label("Mauszeiger übertragen");
                            ui.checkbox(&mut self.config.capture_cursor, "anzeigen");
                            ui.end_row();

                            ui.label("Systemton übertragen");
                            ui.checkbox(&mut self.config.audio_enabled, "aktiv");
                            ui.end_row();

                            ui.label("Bedienung übertragen");
                            ui.checkbox(
                                &mut self.config.control_enabled,
                                "Touch und Tesla-Tastatur aktiv",
                            );
                            ui.end_row();
                        });

                    ui.add_space(12.0);
                    ui.separator();
                    ui.heading("HTTPS und Let's Encrypt");
                    ui.checkbox(
                        &mut self.config.https_enabled,
                        "HTTPS mit automatischem Let's-Encrypt-Zertifikat",
                    );
                    if self.config.https_enabled {
                        ui.add_space(6.0);
                        egui::Grid::new("https-settings")
                            .num_columns(2)
                            .spacing([16.0, 10.0])
                            .show(ui, |ui| {
                                ui.label("Domain");
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.config.https_domain)
                                        .hint_text("screen.example.de")
                                        .desired_width(330.0),
                                );
                                ui.end_row();

                                ui.label("Let's-Encrypt-E-Mail");
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.config.acme_email)
                                        .hint_text("admin@example.de")
                                        .desired_width(330.0),
                                );
                                ui.end_row();

                                ui.label("DNS-Provider");
                                let selected_provider = provider(&self.config.dns_provider)
                                    .map(|provider| provider.name)
                                    .unwrap_or("Unbekannt");
                                egui::ComboBox::from_id_salt("dns-provider")
                                    .selected_text(selected_provider)
                                    .width(260.0)
                                    .show_ui(ui, |ui| {
                                        for definition in PROVIDERS {
                                            ui.selectable_value(
                                                &mut self.config.dns_provider,
                                                definition.code.to_owned(),
                                                definition.name,
                                            );
                                        }
                                    });
                                ui.end_row();
                            });

                        if let Some(definition) = provider(&self.config.dns_provider) {
                            let credentials =
                                self.secrets.credentials_mut(&self.config.dns_provider);
                            egui::Grid::new("dns-credentials")
                                .num_columns(2)
                                .spacing([16.0, 10.0])
                                .show(ui, |ui| {
                                    for field in definition.fields {
                                        ui.label(field.label);
                                        let value = credentials
                                            .entry(field.environment.to_owned())
                                            .or_default();
                                        ui.add(
                                            egui::TextEdit::singleline(value)
                                                .password(true)
                                                .desired_width(330.0),
                                        );
                                        ui.end_row();
                                    }
                                });
                        }

                        ui.checkbox(
                            &mut self.config.acme_accept_tos,
                            "Ich akzeptiere die Let's-Encrypt-Nutzungsbedingungen",
                        );
                        ui.small(
                            "Der API-Schlüssel wird mit Windows DPAPI verschlüsselt. Der ACME-Client wird beim ersten HTTPS-Start geprüft heruntergeladen; Zertifikate werden alle 12 Stunden geprüft und rechtzeitig erneuert.",
                        );
                        ui.small(
                            "Wichtig: Die Domain muss im Tesla-Netz auf die IP dieses Windows-PCs auflösen.",
                        );
                    }

                    ui.add_space(10.0);
                    if ui.button("Einstellungen speichern").clicked() {
                        self.message = Some(match self.save_settings() {
                            Ok(path) => (
                                true,
                                format!("Einstellungen gespeichert: {}", path.display()),
                            ),
                            Err(error) => (false, format!("Speichern fehlgeschlagen: {error}")),
                        });
                    }
                });
                if self.running.is_some() || self.pending_start.is_some() {
                    ui.small("Zum Ändern der Einstellungen zuerst den Stream stoppen.");
                }
            });
        self.show_settings = open;
    }
}

impl eframe::App for SenderApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        context.request_repaint_after(Duration::from_millis(250));
        self.poll_pending_start();

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

        if let Some(running) = &mut self.running
            && let Some(result) = running
                .audio
                .as_mut()
                .and_then(AudioSession::wait_if_finished)
        {
            let message = match result {
                Ok(()) => "Die Systemton-Aufnahme wurde beendet.".to_owned(),
                Err(error) => format!("Systemton-Aufnahme abgebrochen: {error}"),
            };
            if let Some(running) = self.running.take() {
                let _ = running.capture.stop();
                running.server.stop();
            }
            self.message = Some((false, message));
        }

        if let Some(running) = &self.running {
            let now = Instant::now();
            let elapsed = now.duration_since(self.fps_sample_at);
            if elapsed >= Duration::from_millis(500) {
                let frames = running.state.snapshot().frames;
                self.measured_fps =
                    frames.saturating_sub(self.fps_sample_frames) as f64 / elapsed.as_secs_f64();
                self.fps_sample_frames = frames;
                self.fps_sample_at = now;
            }
        } else {
            self.measured_fps = 0.0;
        }

        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Tesla Screen Sender");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(
                            self.pending_start.is_none(),
                            egui::Button::new("⚙ Einstellungen"),
                        )
                        .clicked()
                    {
                        self.show_settings = true;
                    }
                });
            });
            ui.label("Eigenständige Monitorübertragung für den Tesla-Browser im lokalen Netzwerk");
            ui.add_space(12.0);

            let selected_monitor = self
                .displays
                .iter()
                .find(|display| display.index == self.config.monitor_index)
                .map(DisplayInfo::label)
                .unwrap_or_else(|| "Kein Bildschirm".to_owned());
            ui.label(format!("Bildschirm: {selected_monitor}"));
            ui.label(format!(
                "max. {} FPS · {} kbit/s · Ton {} · Bedienung {} · {}",
                self.config.fps,
                self.config.bitrate_kbps,
                if self.config.audio_enabled { "an" } else { "aus" },
                if self.config.control_enabled {
                    "an"
                } else {
                    "aus"
                },
                if self.config.https_enabled {
                    format!("HTTPS ({})", self.config.https_domain)
                } else {
                    "HTTP".to_owned()
                }
            ));

            ui.add_space(12.0);
            if self.running.is_some() {
                if ui.add_sized([150.0, 36.0], egui::Button::new("Stream stoppen")).clicked() {
                    self.stop();
                }
            } else if self.pending_start.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Zertifikat wird vorbereitet …");
                });
            } else if ui
                .add_sized([150.0, 36.0], egui::Button::new("Stream starten"))
                .clicked()
            {
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
                    "{} Browser verbunden · {:.1} FPS tatsächlich · {} Frames · {} Audiopakete · {} × {}",
                    snapshot.clients,
                    self.measured_fps,
                    snapshot.frames,
                    snapshot.audio_packets,
                    snapshot.width,
                    snapshot.height
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
        self.settings_window(&context);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop();
        if let Err(error) = self.save_settings() {
            tracing::error!(%error, "settings could not be saved on exit");
        }
    }
}

pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 560.0])
            .with_min_inner_size([680.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Tesla Screen Sender",
        options,
        Box::new(|context| Ok(Box::new(SenderApp::new(context)))),
    )
}
