const login = document.querySelector("#login");
const viewer = document.querySelector("#viewer");
const loginForm = document.querySelector("#login-form");
const pinInput = document.querySelector("#pin");
const loginError = document.querySelector("#login-error");
const canvas = document.querySelector("#screen");
const status = document.querySelector("#status");
const fullscreenButton = document.querySelector("#fullscreen");
const audioEnableButton = document.querySelector("#audio-enable");
const modeNote = document.querySelector("#mode-note");
const context = canvas.getContext("2d", { alpha: false, desynchronized: true });
const hasWebCodecs = "VideoDecoder" in window && "EncodedVideoChunk" in window;
const streamFormat = hasWebCodecs ? "h264" : "jpeg";

let decoder;
let socket;
let reconnectTimer;
let token = sessionStorage.getItem("tesla-screen-token") || "";
let consecutiveConnectFailures = 0;
let pendingJpeg = null;
let jpegDecodeRunning = false;
let pendingVideoFrame = null;
let videoPaintScheduled = false;
let waitingForKeyframe = true;
const MAX_DECODE_QUEUE = 4;
const MEDIA_LATENCY_SECONDS = 0.15;
let audioContext = null;
let audioAnchorTimestamp = null;
let audioAnchorTime = 0;
let nextAudioTime = 0;

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
      if (pendingVideoFrame) pendingVideoFrame.close();
      pendingVideoFrame = frame;
      scheduleVideoPaint();
    },
    error(error) {
      console.error("Decoder error", error);
      decoder = null;
      waitingForKeyframe = true;
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
  if (data.byteLength < 25) return null;
  const magic = view.getUint32(0);
  const format = magic === 0x54535331 ? "h264" : magic === 0x5453534a ? "jpeg" : magic === 0x54535341 ? "pcm" : null;
  if (!format) return null;
  const width = view.getUint32(4);
  const height = view.getUint32(8);
  const timestamp = view.getUint32(12) * 4294967296 + view.getUint32(16);
  const keyframe = view.getUint8(20) === 1;
  const length = view.getUint32(21);
  if (25 + length !== data.byteLength) return null;
  return { format, width, height, timestamp, keyframe, bytes: new Uint8Array(data, 25, length) };
}

function sizeCanvas(width, height) {
  if (canvas.width !== width || canvas.height !== height) {
    canvas.width = width;
    canvas.height = height;
  }
}

async function drawJpeg(bytes) {
  const blob = new Blob([bytes], { type: "image/jpeg" });
  if ("createImageBitmap" in window) {
    const bitmap = await createImageBitmap(blob);
    sizeCanvas(bitmap.width, bitmap.height);
    context.drawImage(bitmap, 0, 0, canvas.width, canvas.height);
    bitmap.close();
    return;
  }

  await new Promise((resolve, reject) => {
    const objectUrl = URL.createObjectURL(blob);
    const image = new Image();
    image.onload = () => {
      sizeCanvas(image.naturalWidth, image.naturalHeight);
      context.drawImage(image, 0, 0, canvas.width, canvas.height);
      URL.revokeObjectURL(objectUrl);
      resolve();
    };
    image.onerror = () => {
      URL.revokeObjectURL(objectUrl);
      reject(new Error("JPEG decode failed"));
    };
    image.src = objectUrl;
  });
}

function scheduleVideoPaint() {
  if (videoPaintScheduled) return;
  videoPaintScheduled = true;
  requestAnimationFrame(() => {
    videoPaintScheduled = false;
    const frame = pendingVideoFrame;
    if (!frame) return;
    if (audioContext?.state === "running" && audioAnchorTimestamp !== null) {
      const targetTime = audioAnchorTime + (frame.timestamp - audioAnchorTimestamp) / 1_000_000;
      const delay = targetTime - audioContext.currentTime;
      if (delay > 0.025) {
        setTimeout(scheduleVideoPaint, Math.min(50, Math.max(4, delay * 1000 - 8)));
        return;
      }
    }
    pendingVideoFrame = null;
    sizeCanvas(frame.displayWidth, frame.displayHeight);
    context.drawImage(frame, 0, 0, canvas.width, canvas.height);
    frame.close();
    showStatus("Verbunden", true);
    if (pendingVideoFrame) scheduleVideoPaint();
  });
}

function resetMediaClock() {
  audioAnchorTimestamp = null;
  audioAnchorTime = 0;
  nextAudioTime = 0;
}

async function ensureAudioContext() {
  if (!audioContext) {
    const AudioContextClass = window.AudioContext || window.webkitAudioContext;
    if (!AudioContextClass) {
      audioEnableButton.hidden = true;
      return;
    }
    audioContext = new AudioContextClass({ latencyHint: "interactive" });
  }
  try {
    await audioContext.resume();
  } catch (error) {
    console.warn("Audio activation failed", error);
  }
  audioEnableButton.hidden = audioContext.state === "running";
}

function queueAudio(frame) {
  if (!audioContext || audioContext.state !== "running") {
    audioEnableButton.hidden = false;
    return;
  }
  const sampleRate = frame.width;
  const channels = frame.height;
  const frameCount = Math.floor(frame.bytes.byteLength / (channels * 2));
  if (sampleRate < 8_000 || channels < 1 || channels > 8 || frameCount === 0) return;

  const now = audioContext.currentTime;
  if (audioAnchorTimestamp === null) {
    audioAnchorTimestamp = frame.timestamp;
    audioAnchorTime = now + MEDIA_LATENCY_SECONDS;
    nextAudioTime = audioAnchorTime;
  }
  let scheduledTime = audioAnchorTime + (frame.timestamp - audioAnchorTimestamp) / 1_000_000;
  if (scheduledTime < now - 0.05 || scheduledTime > now + 1) {
    audioAnchorTimestamp = frame.timestamp;
    audioAnchorTime = now + MEDIA_LATENCY_SECONDS;
    nextAudioTime = audioAnchorTime;
    scheduledTime = audioAnchorTime;
  }
  scheduledTime = Math.max(scheduledTime, nextAudioTime, now + 0.02);

  const audioBuffer = audioContext.createBuffer(channels, frameCount, sampleRate);
  const view = new DataView(frame.bytes.buffer, frame.bytes.byteOffset, frame.bytes.byteLength);
  for (let channel = 0; channel < channels; channel += 1) {
    const output = audioBuffer.getChannelData(channel);
    for (let sample = 0; sample < frameCount; sample += 1) {
      output[sample] = view.getInt16((sample * channels + channel) * 2, true) / 32768;
    }
  }
  const source = audioContext.createBufferSource();
  source.buffer = audioBuffer;
  source.connect(audioContext.destination);
  source.start(scheduledTime);
  nextAudioTime = scheduledTime + frameCount / sampleRate;
}

async function waitForMediaTime(timestamp) {
  if (audioContext?.state !== "running" || audioAnchorTimestamp === null) return;
  const targetTime = audioAnchorTime + (timestamp - audioAnchorTimestamp) / 1_000_000;
  const delayMs = (targetTime - audioContext.currentTime) * 1000;
  if (delayMs > 4) {
    await new Promise((resolve) => setTimeout(resolve, Math.min(250, delayMs)));
  }
}

function queueJpeg(frame) {
  pendingJpeg = { bytes: frame.bytes.slice(), timestamp: frame.timestamp };
  if (jpegDecodeRunning) return;
  jpegDecodeRunning = true;
  (async () => {
    while (pendingJpeg) {
      const nextJpeg = pendingJpeg;
      pendingJpeg = null;
      try {
        await waitForMediaTime(nextJpeg.timestamp);
        if (pendingJpeg) continue;
        await drawJpeg(nextJpeg.bytes);
        showStatus("Verbunden (HTTP-Kompatibilitätsmodus)", true);
      } catch (error) {
        console.error("JPEG frame decode failed", error);
        showStatus("JPEG-Frame konnte nicht angezeigt werden …");
      }
    }
    jpegDecodeRunning = false;
  })();
}

function connect() {
  clearTimeout(reconnectTimer);
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  socket = new WebSocket(`${scheme}://${location.host}/ws?token=${encodeURIComponent(token)}&format=${streamFormat}`);
  socket.binaryType = "arraybuffer";
  let opened = false;
  socket.onopen = () => {
    opened = true;
    consecutiveConnectFailures = 0;
    showStatus(streamFormat === "jpeg" ? "Warte auf den HTTP-Kompatibilitätsstream …" : "Warte auf den ersten Frame …");
  };
  socket.onmessage = (event) => {
    const frame = parseFrame(event.data);
    if (!frame) return;
    if (frame.format === "pcm") {
      queueAudio(frame);
      return;
    }
    if (frame.format === "jpeg") {
      queueJpeg(frame);
      return;
    }
    if (waitingForKeyframe && !frame.keyframe) return;
    if (frame.keyframe && (!decoder || decoder.state === "closed")) {
      configureDecoder(extractCodecFromAnnexB(frame.bytes));
    }
    if (!decoder || decoder.state !== "configured") return;
    try {
      if (decoder.decodeQueueSize > MAX_DECODE_QUEUE) {
        decoder.reset();
        waitingForKeyframe = !frame.keyframe;
        if (waitingForKeyframe) {
          showStatus("Bildpuffer wird für Echtzeit geleert …");
          return;
        }
      }
      waitingForKeyframe = false;
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
    resetMediaClock();
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
  void ensureAudioContext();
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
  void ensureAudioContext();
  try {
    if (!document.fullscreenElement) await viewer.requestFullscreen();
    else await document.exitFullscreen();
  } catch (error) {
    console.warn("Fullscreen unavailable", error);
  }
});

audioEnableButton.addEventListener("click", () => {
  void ensureAudioContext();
});

if (!hasWebCodecs) {
  modeNote.hidden = false;
  modeNote.textContent = "HTTP-Kompatibilitätsmodus aktiv. Der Stream funktioniert ohne WebCodecs und ohne HTTPS-Zertifikat.";
}

if (token) {
  login.hidden = true;
  viewer.hidden = false;
  connect();
}
