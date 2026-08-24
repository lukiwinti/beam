use axum::{
    Json, Router,
    extract::{ConnectInfo, State, WebSocketUpgrade, ws::Message},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::{
    net::{SocketAddr, TcpListener},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
};
use tokio::sync::{broadcast, oneshot};

const INDEX_HTML: &str = include_str!("../assets/index.html");
const APP_JS: &str = include_str!("../assets/app.js");
const STYLE_CSS: &str = include_str!("../assets/style.css");
const FRAME_MAGIC: &[u8; 4] = b"BWS1";
const FRAME_HEADER_LEN: usize = 25;

#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub width: u32,
    pub height: u32,
    pub timestamp_us: u64,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

impl EncodedFrame {
    pub fn to_wire(&self) -> Vec<u8> {
        let mut packet = Vec::with_capacity(FRAME_HEADER_LEN + self.data.len());
        packet.extend_from_slice(FRAME_MAGIC);
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
    pub frames: u64,
    pub width: u32,
    pub height: u32,
}

pub struct StreamState {
    frame_tx: broadcast::Sender<Arc<EncodedFrame>>,
    latest_keyframe: Mutex<Option<Arc<EncodedFrame>>>,
    pin: String,
    token: String,
    clients: AtomicUsize,
    frames: AtomicU64,
    width: AtomicU32,
    height: AtomicU32,
}

impl StreamState {
    pub fn new(pin: String) -> Arc<Self> {
        let (frame_tx, _) = broadcast::channel(8);
        let token = format!("{:032x}", rand::rng().random::<u128>());
        Arc::new(Self {
            frame_tx,
            latest_keyframe: Mutex::new(None),
            pin,
            token,
            clients: AtomicUsize::new(0),
            frames: AtomicU64::new(0),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
        })
    }

    pub fn publish(&self, frame: EncodedFrame) {
        self.width.store(frame.width, Ordering::Relaxed);
        self.height.store(frame.height, Ordering::Relaxed);
        self.frames.fetch_add(1, Ordering::Relaxed);
        let frame = Arc::new(frame);
        if frame.keyframe {
            *self.latest_keyframe.lock().expect("keyframe lock poisoned") =
                Some(Arc::clone(&frame));
        }
        let _ = self.frame_tx.send(frame);
    }

    pub fn snapshot(&self) -> StreamSnapshot {
        StreamSnapshot {
            clients: self.clients.load(Ordering::Relaxed),
            frames: self.frames.load(Ordering::Relaxed),
            width: self.width.load(Ordering::Relaxed),
            height: self.height.load(Ordering::Relaxed),
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
}

impl LocalServer {
    pub fn start(address: SocketAddr, state: Arc<StreamState>) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(address)?;
        listener.set_nonblocking(true)?;
        let actual_address = listener.local_addr()?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let thread = thread::Builder::new()
            .name("beam-local-web".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("failed to create local web runtime");
                runtime.block_on(async move {
                    let listener = tokio::net::TcpListener::from_std(listener)
                        .expect("failed to convert TCP listener");
                    let app = router(state);
                    if let Err(error) = axum::serve(
                        listener,
                        app.into_make_service_with_connect_info::<SocketAddr>(),
                    )
                    .with_graceful_shutdown(async move {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    {
                        tracing::error!(%error, "local sender web server stopped");
                    }
                });
            })?;
        Ok(Self {
            shutdown_tx: Some(shutdown_tx),
            thread: Some(thread),
            address: actual_address,
        })
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
}

async fn websocket(
    State(state): State<Arc<StreamState>>,
    axum::extract::Query(query): axum::extract::Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> Result<Response, StatusCode> {
    if !state.authorized(&query.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(ws
        .max_message_size(16 * 1024 * 1024)
        .on_upgrade(move |socket| stream_socket(socket, state)))
}

async fn stream_socket(mut socket: axum::extract::ws::WebSocket, state: Arc<StreamState>) {
    state.clients.fetch_add(1, Ordering::Relaxed);
    let mut frames = state.frame_tx.subscribe();

    let initial_keyframe = state
        .latest_keyframe
        .lock()
        .expect("keyframe lock poisoned")
        .clone();
    if let Some(frame) = initial_keyframe
        && socket
            .send(Message::Binary(frame.to_wire().into()))
            .await
            .is_err()
    {
        state.clients.fetch_sub(1, Ordering::Relaxed);
        return;
    }

    loop {
        match frames.recv().await {
            Ok(frame) => {
                if socket
                    .send(Message::Binary(frame.to_wire().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    state.clients.fetch_sub(1, Ordering::Relaxed);
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
            width: 1920,
            height: 1080,
            timestamp_us: 42,
            keyframe: true,
            data: vec![0, 0, 0, 1, 0x65],
        };
        let wire = frame.to_wire();
        assert_eq!(&wire[..4], FRAME_MAGIC);
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
    fn stream_state_tracks_frames_and_latest_geometry() {
        let state = StreamState::new("1234".to_owned());
        state.publish(EncodedFrame {
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
        assert!(state.latest_keyframe.lock().unwrap().is_some());
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
}
