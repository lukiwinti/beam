use axum::{
    Json, Router,
    extract::{ConnectInfo, State, WebSocketUpgrade, ws::Message},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use axum_server_dual_protocol::ServerExt as _;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::{
    net::{SocketAddr, TcpListener},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};
use tokio::sync::{broadcast, oneshot};

const INDEX_HTML: &str = include_str!("../assets/index.html");
const APP_JS: &str = include_str!("../assets/app.js");
const STYLE_CSS: &str = include_str!("../assets/style.css");
const FRAME_HEADER_LEN: usize = 25;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamFormat {
    H264,
    Jpeg,
    AudioPcm,
}

impl StreamFormat {
    const fn magic(self) -> &'static [u8; 4] {
        match self {
            Self::H264 => b"TSS1",
            Self::Jpeg => b"TSSJ",
            Self::AudioPcm => b"TSSA",
        }
    }
}

#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub format: StreamFormat,
    pub width: u32,
    pub height: u32,
    pub timestamp_us: u64,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

impl EncodedFrame {
    pub fn to_wire(&self) -> Vec<u8> {
        let mut packet = Vec::with_capacity(FRAME_HEADER_LEN + self.data.len());
        packet.extend_from_slice(self.format.magic());
        packet.extend_from_slice(&self.width.to_be_bytes());
        packet.extend_from_slice(&self.height.to_be_bytes());
        packet.extend_from_slice(&self.timestamp_us.to_be_bytes());
        packet.push(u8::from(self.keyframe));
        packet.extend_from_slice(&(self.data.len() as u32).to_be_bytes());
        packet.extend_from_slice(&self.data);
        packet
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamSnapshot {
    pub clients: usize,
    pub jpeg_clients: usize,
    pub frames: u64,
    pub audio_packets: u64,
    pub width: u32,
    pub height: u32,
    pub control_enabled: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TouchPhase {
    Down,
    Move,
    Up,
    Cancel,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RemoteKey {
    Backspace,
    Delete,
    Enter,
    Tab,
    Escape,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    Home,
    End,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteInputEvent {
    Touch {
        phase: TouchPhase,
        id: u32,
        x: f64,
        y: f64,
    },
    Text {
        text: String,
    },
    Key {
        key: RemoteKey,
    },
    CancelAll,
}

impl RemoteInputEvent {
    fn is_valid(&self) -> bool {
        match self {
            Self::Touch { id, x, y, .. } => {
                *id > 0
                    && *id <= 1_000_000
                    && x.is_finite()
                    && y.is_finite()
                    && (0.0..=1.0).contains(x)
                    && (0.0..=1.0).contains(y)
            }
            Self::Text { text } => !text.contains('\0') && text.encode_utf16().count() <= 1_024,
            Self::Key { .. } => true,
            Self::CancelAll => true,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct NormalizedRect {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlSignal {
    Control {
        enabled: bool,
    },
    Keyboard {
        show: bool,
        rect: Option<NormalizedRect>,
    },
}

pub struct StreamState {
    frame_tx: broadcast::Sender<Arc<EncodedFrame>>,
    latest_h264: Mutex<Option<Arc<EncodedFrame>>>,
    latest_jpeg: Mutex<Option<Arc<EncodedFrame>>>,
    pin: String,
    token: String,
    clients: AtomicUsize,
    jpeg_clients: AtomicUsize,
    frames: AtomicU64,
    audio_packets: AtomicU64,
    width: AtomicU32,
    height: AtomicU32,
    input_tx: Mutex<Option<mpsc::Sender<RemoteInputEvent>>>,
    control_tx: broadcast::Sender<ControlSignal>,
}

impl StreamState {
    pub fn new(pin: String) -> Arc<Self> {
        let (frame_tx, _) = broadcast::channel(64);
        let (control_tx, _) = broadcast::channel(16);
        let token = format!("{:032x}", rand::rng().random::<u128>());
        Arc::new(Self {
            frame_tx,
            latest_h264: Mutex::new(None),
            latest_jpeg: Mutex::new(None),
            pin,
            token,
            clients: AtomicUsize::new(0),
            jpeg_clients: AtomicUsize::new(0),
            frames: AtomicU64::new(0),
            audio_packets: AtomicU64::new(0),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            input_tx: Mutex::new(None),
            control_tx,
        })
    }

    pub fn publish(&self, frame: EncodedFrame) {
        match frame.format {
            StreamFormat::H264 => {
                self.width.store(frame.width, Ordering::Relaxed);
                self.height.store(frame.height, Ordering::Relaxed);
                self.frames.fetch_add(1, Ordering::Relaxed);
            }
            StreamFormat::Jpeg => {
                self.width.store(frame.width, Ordering::Relaxed);
                self.height.store(frame.height, Ordering::Relaxed);
            }
            StreamFormat::AudioPcm => {
                self.audio_packets.fetch_add(1, Ordering::Relaxed);
            }
        }
        let frame = Arc::new(frame);
        match frame.format {
            StreamFormat::H264 if frame.keyframe => {
                *self.latest_h264.lock().expect("H.264 frame lock poisoned") =
                    Some(Arc::clone(&frame));
            }
            StreamFormat::Jpeg => {
                *self.latest_jpeg.lock().expect("JPEG frame lock poisoned") =
                    Some(Arc::clone(&frame));
            }
            StreamFormat::AudioPcm => {}
            StreamFormat::H264 => {}
        }
        let _ = self.frame_tx.send(frame);
    }

    pub fn needs_jpeg_frames(&self) -> bool {
        self.jpeg_clients.load(Ordering::Relaxed) > 0
    }

    pub fn snapshot(&self) -> StreamSnapshot {
        StreamSnapshot {
            clients: self.clients.load(Ordering::Relaxed),
            jpeg_clients: self.jpeg_clients.load(Ordering::Relaxed),
            frames: self.frames.load(Ordering::Relaxed),
            audio_packets: self.audio_packets.load(Ordering::Relaxed),
            width: self.width.load(Ordering::Relaxed),
            height: self.height.load(Ordering::Relaxed),
            control_enabled: self.control_enabled(),
        }
    }

    pub fn enable_control(&self, input_tx: mpsc::Sender<RemoteInputEvent>) {
        *self.input_tx.lock().expect("input sender lock poisoned") = Some(input_tx);
        self.publish_control(ControlSignal::Control { enabled: true });
    }

    pub fn disable_control(&self) {
        self.input_tx
            .lock()
            .expect("input sender lock poisoned")
            .take();
        self.publish_control(ControlSignal::Control { enabled: false });
        self.publish_control(ControlSignal::Keyboard {
            show: false,
            rect: None,
        });
    }

    pub fn publish_control(&self, signal: ControlSignal) {
        let _ = self.control_tx.send(signal);
    }

    fn control_enabled(&self) -> bool {
        self.input_tx
            .lock()
            .expect("input sender lock poisoned")
            .is_some()
    }

    fn dispatch_input(&self, event: RemoteInputEvent) {
        if !event.is_valid() {
            return;
        }
        let sender = self
            .input_tx
            .lock()
            .expect("input sender lock poisoned")
            .clone();
        if let Some(sender) = sender {
            let _ = sender.send(event);
        }
    }

    fn authorized(&self, token: &str) -> bool {
        constant_time_eq(token.as_bytes(), self.token.as_bytes())
    }
}

pub struct LocalServer {
    shutdown_tx: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
    pub address: SocketAddr,
    tls_config: Option<axum_server::tls_rustls::RustlsConfig>,
}

#[derive(Debug, Clone)]
pub struct TlsIdentity {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

impl LocalServer {
    pub fn start(
        address: SocketAddr,
        state: Arc<StreamState>,
        tls: Option<TlsIdentity>,
    ) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(address)?;
        listener.set_nonblocking(true)?;
        let actual_address = listener.local_addr()?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("tesla-screen-local-web".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("failed to create local web runtime");
                runtime.block_on(async move {
                    let app = router(state);
                    if let Some(identity) = tls {
                        let config = match axum_server::tls_rustls::RustlsConfig::from_pem_file(
                            &identity.cert_path,
                            &identity.key_path,
                        )
                        .await
                        {
                            Ok(config) => config,
                            Err(error) => {
                                let _ = ready_tx.send(Err(format!(
                                    "TLS-Zertifikat konnte nicht geladen werden: {error}"
                                )));
                                return;
                            }
                        };
                        let handle = axum_server::Handle::new();
                        let shutdown_handle = handle.clone();
                        tokio::spawn(async move {
                            let _ = shutdown_rx.await;
                            shutdown_handle.graceful_shutdown(Some(Duration::from_secs(3)));
                        });
                        let server = axum_server_dual_protocol::from_tcp_dual_protocol(
                            listener,
                            config.clone(),
                        );
                        let _ = ready_tx.send(Ok(Some(config.clone())));
                        if let Err(error) = server
                            .set_upgrade(true)
                            .handle(handle)
                            .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                            .await
                        {
                            tracing::error!(%error, "local HTTPS sender web server stopped");
                        }
                    } else {
                        let listener = tokio::net::TcpListener::from_std(listener)
                            .expect("failed to convert TCP listener");
                        let _ = ready_tx.send(Ok(None));
                        if let Err(error) = axum::serve(
                            listener,
                            app.into_make_service_with_connect_info::<SocketAddr>(),
                        )
                        .with_graceful_shutdown(async move {
                            let _ = shutdown_rx.await;
                        })
                        .await
                        {
                            tracing::error!(%error, "local HTTP sender web server stopped");
                        }
                    }
                });
            })?;
        let tls_config = match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(config)) => config,
            Ok(Err(error)) => {
                let _ = thread.join();
                anyhow::bail!(error)
            }
            Err(error) => {
                let _ = shutdown_tx.send(());
                let _ = thread.join();
                anyhow::bail!("Webserver hat nicht rechtzeitig geantwortet: {error}")
            }
        };
        Ok(Self {
            shutdown_tx: Some(shutdown_tx),
            thread: Some(thread),
            address: actual_address,
            tls_config,
        })
    }

    pub fn tls_config(&self) -> Option<axum_server::tls_rustls::RustlsConfig> {
        self.tls_config.clone()
    }

    pub fn stop(mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
    }
}

fn router(state: Arc<StreamState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .route("/api/status", get(status))
        .route("/api/login", post(login))
        .route("/ws", get(websocket))
        .with_state(state)
}

async fn index() -> Response {
    static_response("text/html; charset=utf-8", INDEX_HTML)
}

async fn app_js() -> Response {
    static_response("text/javascript; charset=utf-8", APP_JS)
}

async fn style_css() -> Response {
    static_response("text/css; charset=utf-8", STYLE_CSS)
}

fn static_response(content_type: &'static str, body: &'static str) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; connect-src 'self' ws: wss:; img-src 'self' data: blob:; style-src 'self'; script-src 'self'; object-src 'none'; frame-ancestors 'none'",
        ),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    (headers, body).into_response()
}

async fn status(State(state): State<Arc<StreamState>>) -> Json<StreamSnapshot> {
    Json(state.snapshot())
}

#[derive(Deserialize)]
struct LoginRequest {
    pin: String,
}

#[derive(Serialize)]
struct LoginResponse {
    token: String,
}

async fn login(
    State(state): State<Arc<StreamState>>,
    ConnectInfo(_peer): ConnectInfo<SocketAddr>,
    Json(request): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, StatusCode> {
    if !constant_time_eq(request.pin.as_bytes(), state.pin.as_bytes()) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(Json(LoginResponse {
        token: state.token.clone(),
    }))
}

#[derive(Deserialize)]
struct WsQuery {
    token: String,
    format: Option<String>,
}

async fn websocket(
    State(state): State<Arc<StreamState>>,
    axum::extract::Query(query): axum::extract::Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> Result<Response, StatusCode> {
    if !state.authorized(&query.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let format = match query.format.as_deref() {
        None | Some("h264") => StreamFormat::H264,
        Some("jpeg") => StreamFormat::Jpeg,
        Some(_) => return Err(StatusCode::BAD_REQUEST),
    };
    Ok(ws
        .max_message_size(16 * 1024 * 1024)
        .on_upgrade(move |socket| stream_socket(socket, state, format)))
}

async fn stream_socket(
    mut socket: axum::extract::ws::WebSocket,
    state: Arc<StreamState>,
    format: StreamFormat,
) {
    state.clients.fetch_add(1, Ordering::Relaxed);
    if format == StreamFormat::Jpeg {
        state.jpeg_clients.fetch_add(1, Ordering::Relaxed);
    }
    let mut frames = state.frame_tx.subscribe();
    let mut controls = state.control_tx.subscribe();

    if send_control_signal(
        &mut socket,
        &ControlSignal::Control {
            enabled: state.control_enabled(),
        },
    )
    .await
    .is_err()
    {
        decrement_clients(&state, format);
        return;
    }

    let initial_frame = match format {
        StreamFormat::H264 => state
            .latest_h264
            .lock()
            .expect("H.264 frame lock poisoned")
            .clone(),
        StreamFormat::Jpeg => state
            .latest_jpeg
            .lock()
            .expect("JPEG frame lock poisoned")
            .clone(),
        StreamFormat::AudioPcm => None,
    };
    if let Some(frame) = initial_frame
        && socket
            .send(Message::Binary(frame.to_wire().into()))
            .await
            .is_err()
    {
        decrement_clients(&state, format);
        return;
    }

    let mut waiting_for_keyframe = false;
    loop {
        tokio::select! {
            frame = frames.recv() => match frame {
                Ok(frame) => {
                if frame.format != format && frame.format != StreamFormat::AudioPcm {
                    continue;
                }
                if waiting_for_keyframe && frame.format == StreamFormat::H264 && !frame.keyframe {
                    continue;
                }
                if frame.format == StreamFormat::H264 && frame.keyframe {
                    waiting_for_keyframe = false;
                }
                if socket
                    .send(Message::Binary(frame.to_wire().into()))
                    .await
                    .is_err()
                {
                    break;
                }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                waiting_for_keyframe = format == StreamFormat::H264;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(event) = serde_json::from_str::<RemoteInputEvent>(text.as_str()) {
                        state.dispatch_input(event);
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
            signal = controls.recv() => match signal {
                Ok(signal) => {
                    if send_control_signal(&mut socket, &signal).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
    state.dispatch_input(RemoteInputEvent::CancelAll);
    decrement_clients(&state, format);
}

async fn send_control_signal(
    socket: &mut axum::extract::ws::WebSocket,
    signal: &ControlSignal,
) -> Result<(), axum::Error> {
    let json = serde_json::to_string(signal).expect("control signal serialization failed");
    socket.send(Message::Text(json.into())).await
}

fn decrement_clients(state: &StreamState, format: StreamFormat) {
    state.clients.fetch_sub(1, Ordering::Relaxed);
    if format == StreamFormat::Jpeg {
        state.jpeg_clients.fetch_sub(1, Ordering::Relaxed);
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let max_len = left.len().max(right.len());
    let mut difference = u8::from(left.len() != right.len());
    for index in 0..max_len {
        let left_byte = left.get(index).copied().unwrap_or(0);
        let right_byte = right.get(index).copied().unwrap_or(0);
        difference |= left_byte ^ right_byte;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[test]
    fn frame_wire_header_is_stable() {
        let frame = EncodedFrame {
            format: StreamFormat::H264,
            width: 1920,
            height: 1080,
            timestamp_us: 42,
            keyframe: true,
            data: vec![0, 0, 0, 1, 0x65],
        };
        let wire = frame.to_wire();
        assert_eq!(&wire[..4], b"TSS1");
        assert_eq!(u32::from_be_bytes(wire[4..8].try_into().unwrap()), 1920);
        assert_eq!(u32::from_be_bytes(wire[8..12].try_into().unwrap()), 1080);
        assert_eq!(u64::from_be_bytes(wire[12..20].try_into().unwrap()), 42);
        assert_eq!(wire[20], 1);
        assert_eq!(u32::from_be_bytes(wire[21..25].try_into().unwrap()), 5);
        assert_eq!(&wire[25..], frame.data);
    }

    #[test]
    fn constant_time_comparison_handles_length_mismatch() {
        assert!(constant_time_eq(b"123456", b"123456"));
        assert!(!constant_time_eq(b"123456", b"123457"));
        assert!(!constant_time_eq(b"x", &[b'x'; 257]));
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[test]
    fn remote_input_is_validated_and_dispatched_only_when_enabled() {
        let state = StreamState::new("1234".to_owned());
        let valid = RemoteInputEvent::Touch {
            phase: TouchPhase::Down,
            id: 1,
            x: 0.25,
            y: 0.75,
        };
        let invalid = RemoteInputEvent::Touch {
            phase: TouchPhase::Move,
            id: 2,
            x: -0.1,
            y: 1.2,
        };
        state.dispatch_input(valid.clone());

        let (input_tx, input_rx) = mpsc::channel();
        state.enable_control(input_tx);
        assert!(state.snapshot().control_enabled);
        state.dispatch_input(invalid);
        state.dispatch_input(valid.clone());
        assert_eq!(
            input_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            valid
        );
        assert!(input_rx.try_recv().is_err());

        state.disable_control();
        assert!(!state.snapshot().control_enabled);
    }

    #[test]
    fn stream_state_tracks_frames_and_latest_geometry() {
        let state = StreamState::new("1234".to_owned());
        state.publish(EncodedFrame {
            format: StreamFormat::H264,
            width: 1280,
            height: 720,
            timestamp_us: 1,
            keyframe: true,
            data: vec![1, 2, 3],
        });
        let snapshot = state.snapshot();
        assert_eq!(snapshot.frames, 1);
        assert_eq!(snapshot.width, 1280);
        assert_eq!(snapshot.height, 720);
        assert_eq!(snapshot.clients, 0);
        assert_eq!(snapshot.jpeg_clients, 0);
        assert!(state.latest_h264.lock().unwrap().is_some());
    }

    #[test]
    fn jpeg_frames_use_compatibility_magic_and_do_not_increment_h264_counter() {
        let state = StreamState::new("1234".to_owned());
        let frame = EncodedFrame {
            format: StreamFormat::Jpeg,
            width: 800,
            height: 600,
            timestamp_us: 2,
            keyframe: true,
            data: vec![0xff, 0xd8, 0xff, 0xd9],
        };
        assert_eq!(&frame.to_wire()[..4], b"TSSJ");
        state.publish(frame);
        assert_eq!(state.snapshot().frames, 0);
        assert!(state.latest_jpeg.lock().unwrap().is_some());
    }

    #[test]
    fn pcm_audio_uses_own_magic_and_preserves_video_geometry() {
        let state = StreamState::new("1234".to_owned());
        state.publish(EncodedFrame {
            format: StreamFormat::H264,
            width: 1920,
            height: 1080,
            timestamp_us: 1,
            keyframe: true,
            data: vec![1],
        });
        let audio = EncodedFrame {
            format: StreamFormat::AudioPcm,
            width: 48_000,
            height: 2,
            timestamp_us: 2,
            keyframe: true,
            data: vec![0, 0, 0, 0],
        };
        assert_eq!(&audio.to_wire()[..4], b"TSSA");
        state.publish(audio);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.audio_packets, 1);
        assert_eq!((snapshot.width, snapshot.height), (1920, 1080));
    }

    #[tokio::test]
    async fn login_rejects_bad_pin_and_returns_token_for_good_pin() {
        let state = StreamState::new("correct-pin".to_owned());
        let bad_request = Request::builder()
            .method("POST")
            .uri("/api/login")
            .header(header::CONTENT_TYPE, "application/json")
            .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))))
            .body(Body::from(r#"{"pin":"wrong"}"#))
            .unwrap();
        let bad_response = router(Arc::clone(&state))
            .oneshot(bad_request)
            .await
            .unwrap();
        assert_eq!(bad_response.status(), StatusCode::UNAUTHORIZED);

        let good_request = Request::builder()
            .method("POST")
            .uri("/api/login")
            .header(header::CONTENT_TYPE, "application/json")
            .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))))
            .body(Body::from(r#"{"pin":"correct-pin"}"#))
            .unwrap();
        let good_response = router(state).oneshot(good_request).await.unwrap();
        assert_eq!(good_response.status(), StatusCode::OK);
        let body = good_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["token"].as_str().unwrap().len(), 32);
    }

    #[tokio::test]
    async fn static_assets_have_no_store_and_security_headers() {
        let request = Request::builder().uri("/").body(Body::empty()).unwrap();
        let response = router(StreamState::new("1234".to_owned()))
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "no-store, max-age=0"
        );
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        assert!(
            response
                .headers()
                .contains_key(header::CONTENT_SECURITY_POLICY)
        );
    }

    #[test]
    fn local_https_server_serves_embedded_frontend() {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let directory = std::env::temp_dir().join(format!(
            "tesla-screen-tls-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let cert_path = directory.join("cert.pem");
        let key_path = directory.join("key.pem");
        std::fs::write(&cert_path, cert.pem()).unwrap();
        std::fs::write(&key_path, signing_key.serialize_pem()).unwrap();

        let server = LocalServer::start(
            "127.0.0.1:0".parse().unwrap(),
            StreamState::new("1234".to_owned()),
            Some(TlsIdentity {
                cert_path,
                key_path,
            }),
        )
        .unwrap();
        let client = reqwest::blocking::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();
        let response = client
            .get(format!("https://localhost:{}/", server.address.port()))
            .send()
            .unwrap();
        assert!(response.status().is_success());
        assert!(response.text().unwrap().contains("Tesla Screen"));

        let redirected = client
            .get(format!("http://localhost:{}/", server.address.port()))
            .send()
            .unwrap();
        assert!(redirected.status().is_success());
        assert_eq!(redirected.url().scheme(), "https");
        assert!(redirected.text().unwrap().contains("Tesla Screen"));
        server.stop();
        std::fs::remove_dir_all(directory).unwrap();
    }
}
