const login = document.querySelector("#login");
const viewer = document.querySelector("#viewer");
const loginForm = document.querySelector("#login-form");
const pinInput = document.querySelector("#pin");
const loginError = document.querySelector("#login-error");
const canvas = document.querySelector("#screen");
const status = document.querySelector("#status");
const fullscreenButton = document.querySelector("#fullscreen");
const context = canvas.getContext("2d", { alpha: false, desynchronized: true });

let decoder;
let socket;
let reconnectTimer;
let token = sessionStorage.getItem("tesla-screen-token") || "";
let consecutiveConnectFailures = 0;

function showStatus(message, connected = false) {
  status.textContent = message;
  status.classList.toggle("connected", connected);
}

function extractCodecFromAnnexB(payload) {
  for (let index = 0; index + 4 < payload.length; index += 1) {
    let nalStart = -1;
    if (payload[index] === 0 && payload[index + 1] === 0 && payload[index + 2] === 0 && payload[index + 3] === 1) {
      nalStart = index + 4;
    } else if (payload[index] === 0 && payload[index + 1] === 0 && payload[index + 2] === 1) {
      nalStart = index + 3;
    }
    if (nalStart >= 0 && nalStart + 3 < payload.length && (payload[nalStart] & 0x1f) === 7) {
      const profile = payload[nalStart + 1].toString(16).padStart(2, "0");
      const compatibility = payload[nalStart + 2].toString(16).padStart(2, "0");
      const level = payload[nalStart + 3].toString(16).padStart(2, "0");
      return `avc1.${profile}${compatibility}${level}`;
    }
  }
  return null;
}

function configureDecoder(codec) {
  if (decoder && decoder.state !== "closed") decoder.close();
  decoder = new VideoDecoder({
    output(frame) {
      if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
        canvas.width = frame.displayWidth;
        canvas.height = frame.displayHeight;
      }
      context.drawImage(frame, 0, 0, canvas.width, canvas.height);
      frame.close();
      showStatus("Verbunden", true);
    },
    error(error) {
      console.error("Decoder error", error);
      decoder = null;
      showStatus("Decoder wartet auf ein neues Schlüsselbild …");
    },
  });
  decoder.configure({
    codec: codec || "avc1.4d0033",
    optimizeForLatency: true,
    hardwareAcceleration: "prefer-hardware",
  });
}

function parseFrame(data) {
  const view = new DataView(data);
  if (data.byteLength < 25 || view.getUint32(0) !== 0x42575331) return null;
  const width = view.getUint32(4);
  const height = view.getUint32(8);
  const timestamp = Number(view.getBigUint64(12));
  const keyframe = view.getUint8(20) === 1;
  const length = view.getUint32(21);
  if (25 + length !== data.byteLength) return null;
  return { width, height, timestamp, keyframe, bytes: new Uint8Array(data, 25, length) };
}

function connect() {
  clearTimeout(reconnectTimer);
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  socket = new WebSocket(`${scheme}://${location.host}/ws?token=${encodeURIComponent(token)}`);
  socket.binaryType = "arraybuffer";
  let opened = false;
  socket.onopen = () => {
    opened = true;
    consecutiveConnectFailures = 0;
    showStatus("Warte auf den ersten Frame …");
  };
  socket.onmessage = (event) => {
    const frame = parseFrame(event.data);
    if (!frame) return;
    if (frame.keyframe && (!decoder || decoder.state === "closed")) {
      configureDecoder(extractCodecFromAnnexB(frame.bytes));
    }
    if (!decoder || decoder.state !== "configured") return;
    try {
      decoder.decode(new EncodedVideoChunk({
        type: frame.keyframe ? "key" : "delta",
        timestamp: frame.timestamp,
        data: frame.bytes,
      }));
    } catch (error) {
      console.error("Frame decode failed", error);
    }
  };
  socket.onclose = (event) => {
    if (!opened) consecutiveConnectFailures += 1;
    if (event.code === 1008 || event.code === 1002 || consecutiveConnectFailures >= 3) {
      sessionStorage.removeItem("tesla-screen-token");
      token = "";
      consecutiveConnectFailures = 0;
      viewer.hidden = true;
      login.hidden = false;
      loginError.textContent = "Sitzung abgelaufen. Bitte erneut anmelden.";
      return;
    }
    showStatus("Verbindung unterbrochen – neuer Versuch …");
    reconnectTimer = setTimeout(connect, 1200);
  };
  socket.onerror = () => socket.close();
}

loginForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  loginError.textContent = "";
  try {
    const response = await fetch("/api/login", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ pin: pinInput.value }),
    });
    if (!response.ok) throw new Error("invalid credentials");
    const payload = await response.json();
    token = payload.token;
    sessionStorage.setItem("tesla-screen-token", token);
    login.hidden = true;
    viewer.hidden = false;
    consecutiveConnectFailures = 0;
    connect();
  } catch {
    loginError.textContent = "Ungültige PIN";
    pinInput.select();
    pinInput.focus();
  }
});

fullscreenButton.addEventListener("click", async () => {
  try {
    if (!document.fullscreenElement) await viewer.requestFullscreen();
    else await document.exitFullscreen();
  } catch (error) {
    console.warn("Fullscreen unavailable", error);
  }
});

if (!("VideoDecoder" in window)) {
  loginError.textContent = "Dieser Browser unterstützt WebCodecs nicht.";
  loginForm.querySelector("button").disabled = true;
} else if (token) {
  login.hidden = true;
  viewer.hidden = false;
  connect();
}
