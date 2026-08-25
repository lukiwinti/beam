const login = document.querySelector("#login");
const viewer = document.querySelector("#viewer");
const loginForm = document.querySelector("#login-form");
const pinInput = document.querySelector("#pin");
const loginError = document.querySelector("#login-error");
const canvas = document.querySelector("#screen");
const status = document.querySelector("#status");
const fullscreenButton = document.querySelector("#fullscreen");
const audioEnableButton = document.querySelector("#audio-enable");
const controlState = document.querySelector("#control-state");
const remoteKeyboard = document.querySelector("#remote-keyboard");
const modeNote = document.querySelector("#mode-note");
const context = canvas.getContext("2d", { alpha: false, desynchronized: true });
const hasWebCodecs = "VideoDecoder" in window && "EncodedVideoChunk" in window;
const streamFormat = hasWebCodecs ? "h264" : "jpeg";

let decoder;
let socket;
let reconnectTimer;
let token = sessionStorage.getItem("tesla-screen-token") || "";
let reconnectAttempts = 0;
let rememberedPin = "";
let pendingJpeg = null;
let jpegDecodeRunning = false;
let videoFrameQueue = [];
let videoPaintScheduled = false;
let videoPaintTimer = null;
let waitingForKeyframe = true;
const MAX_DECODE_QUEUE = 4;
const MAX_SYNCED_VIDEO_FRAMES = 12;
const MEDIA_LATENCY_SECONDS = 0.15;
let audioContext = null;
let audioAnchorTimestamp = null;
let audioAnchorTime = 0;
let nextAudioTime = 0;
const scheduledAudioSources = new Set();
const activePointers = new Map();
const KEYBOARD_SENTINEL = "\u200b";
let controlEnabled = false;
let keyboardCandidate = false;
let keyboardConfirmed = false;
let focusedRemoteRect = null;
let composingText = false;
let ignoreNextKeyboardInput = false;
let keyboardShiftPx = 0;

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
  clearVideoFrames();
  decoder = new VideoDecoder({
    output(frame) {
      const audioSyncActive = audioContext?.state === "running" && audioAnchorTimestamp !== null;
      if (!audioSyncActive) clearVideoFrames();
      videoFrameQueue.push(frame);
      while (videoFrameQueue.length > MAX_SYNCED_VIDEO_FRAMES) {
        videoFrameQueue.shift().close();
      }
      scheduleVideoPaint();
    },
    error(error) {
      console.error("Decoder error", error);
      decoder = null;
      waitingForKeyframe = true;
      clearVideoFrames();
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

function clearVideoFrames() {
  for (const frame of videoFrameQueue) frame.close();
  videoFrameQueue = [];
  if (videoPaintTimer !== null) {
    clearTimeout(videoPaintTimer);
    videoPaintTimer = null;
  }
}

function frameTargetTime(frame) {
  return audioAnchorTime + (frame.timestamp - audioAnchorTimestamp) / 1_000_000;
}

function scheduleVideoPaint() {
  if (videoPaintScheduled || videoPaintTimer !== null) return;
  videoPaintScheduled = true;
  requestAnimationFrame(() => {
    videoPaintScheduled = false;
    if (videoFrameQueue.length === 0) return;
    if (audioContext?.state === "running" && audioAnchorTimestamp !== null) {
      while (
        videoFrameQueue.length > 1 &&
        frameTargetTime(videoFrameQueue[0]) < audioContext.currentTime - 0.08
      ) {
        videoFrameQueue.shift().close();
      }
      const targetTime = frameTargetTime(videoFrameQueue[0]);
      const delay = targetTime - audioContext.currentTime;
      if (delay > 0.012) {
        videoPaintTimer = setTimeout(() => {
          videoPaintTimer = null;
          scheduleVideoPaint();
        }, Math.min(50, Math.max(2, delay * 1000 - 5)));
        return;
      }
    } else {
      while (videoFrameQueue.length > 1) videoFrameQueue.shift().close();
    }
    const frame = videoFrameQueue.shift();
    sizeCanvas(frame.displayWidth, frame.displayHeight);
    context.drawImage(frame, 0, 0, canvas.width, canvas.height);
    frame.close();
    showStatus("Verbunden", true);
    if (videoFrameQueue.length > 0) scheduleVideoPaint();
  });
}

function stopScheduledAudio() {
  for (const source of scheduledAudioSources) {
    try {
      source.stop();
    } catch {
      // The source may already have finished between iteration and stop().
    }
  }
  scheduledAudioSources.clear();
}

function resetMediaClock() {
  stopScheduledAudio();
  audioAnchorTimestamp = null;
  audioAnchorTime = 0;
  nextAudioTime = 0;
  pendingJpeg = null;
  clearVideoFrames();
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
    stopScheduledAudio();
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
  source.onended = () => scheduledAudioSources.delete(source);
  scheduledAudioSources.add(source);
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

function sendRemote(message) {
  if (!controlEnabled || !socket || socket.readyState !== WebSocket.OPEN) return false;
  socket.send(JSON.stringify(message));
  return true;
}

function setControlEnabled(enabled) {
  controlEnabled = Boolean(enabled);
  controlState.hidden = !controlEnabled;
  canvas.classList.toggle("interactive", controlEnabled);
  if (!controlEnabled) {
    activePointers.clear();
    dismissRemoteKeyboard();
  }
}

function resetKeyboardBuffer() {
  remoteKeyboard.value = KEYBOARD_SENTINEL;
  try {
    remoteKeyboard.setSelectionRange(KEYBOARD_SENTINEL.length, KEYBOARD_SENTINEL.length);
  } catch {
    // Some vehicle browsers do not expose selection APIs for tiny hidden fields.
  }
}

function armRemoteKeyboard() {
  if (!controlEnabled) return;
  keyboardCandidate = true;
  resetKeyboardBuffer();
  try {
    remoteKeyboard.focus({ preventScroll: true });
  } catch {
    remoteKeyboard.focus();
  }
  try {
    navigator.virtualKeyboard?.show();
  } catch {
    // Focusing the textarea is the broadly supported keyboard trigger.
  }
}

function confirmRemoteKeyboard(rect) {
  keyboardCandidate = true;
  keyboardConfirmed = true;
  focusedRemoteRect = rect || null;
  if (document.activeElement !== remoteKeyboard) armRemoteKeyboard();
  updateKeyboardShift();
}

function dismissRemoteKeyboard() {
  keyboardCandidate = false;
  keyboardConfirmed = false;
  focusedRemoteRect = null;
  keyboardShiftPx = 0;
  viewer.style.setProperty("--keyboard-shift", "0px");
  if (document.activeElement === remoteKeyboard) remoteKeyboard.blur();
  try {
    navigator.virtualKeyboard?.hide();
  } catch {
    // blur() is sufficient on browsers without the Virtual Keyboard API.
  }
}

function updateKeyboardShift() {
  if (!keyboardConfirmed || !focusedRemoteRect || canvas.width === 0 || canvas.height === 0) {
    keyboardShiftPx = 0;
    viewer.style.setProperty("--keyboard-shift", "0px");
    return;
  }
  const viewport = window.visualViewport;
  const availableTop = viewport?.offsetTop || 0;
  const availableBottom = availableTop + (viewport?.height || window.innerHeight);
  const canvasRect = canvas.getBoundingClientRect();
  const scale = Math.min(canvasRect.width / canvas.width, canvasRect.height / canvas.height);
  const contentHeight = canvas.height * scale;
  const contentTop = canvasRect.top - keyboardShiftPx + (canvasRect.height - contentHeight) / 2;
  const focusTop = contentTop + focusedRemoteRect.top * contentHeight;
  const focusBottom = contentTop + focusedRemoteRect.bottom * contentHeight;
  const desiredShift = Math.min(0, availableBottom - 28 - focusBottom);
  const minimumShift = availableTop + 20 - focusTop;
  const shift = Math.round(Math.max(desiredShift, minimumShift));
  keyboardShiftPx = shift;
  viewer.style.setProperty("--keyboard-shift", `${shift}px`);
}

function handleControlMessage(message) {
  if (message.type === "control") {
    setControlEnabled(message.enabled);
    return;
  }
  if (message.type === "keyboard") {
    if (message.show) confirmRemoteKeyboard(message.rect);
    else dismissRemoteKeyboard();
  }
}

function canvasCoordinates(event, clampOutside = false) {
  if (!canvas.width || !canvas.height) return null;
  const rect = canvas.getBoundingClientRect();
  const scale = Math.min(rect.width / canvas.width, rect.height / canvas.height);
  const contentWidth = canvas.width * scale;
  const contentHeight = canvas.height * scale;
  const left = rect.left + (rect.width - contentWidth) / 2;
  const top = rect.top + (rect.height - contentHeight) / 2;
  let x = (event.clientX - left) / contentWidth;
  let y = (event.clientY - top) / contentHeight;
  if (!clampOutside && (x < 0 || x > 1 || y < 0 || y > 1)) return null;
  x = Math.max(0, Math.min(1, x));
  y = Math.max(0, Math.min(1, y));
  return { x, y };
}

function pointerRemoteId(pointerId) {
  return Math.abs(Number(pointerId) % 999_999) + 1;
}

function handlePointerDown(event) {
  if (!controlEnabled || (event.pointerType === "mouse" && event.button !== 0)) return;
  const point = canvasCoordinates(event);
  if (!point) return;
  event.preventDefault();
  try {
    canvas.setPointerCapture(event.pointerId);
  } catch {
    // Pointer capture is optional; document-level pointer events still complete the contact.
  }
  const pointer = {
    id: pointerRemoteId(event.pointerId),
    x: point.x,
    y: point.y,
    startX: event.clientX,
    startY: event.clientY,
    moved: false,
  };
  activePointers.set(event.pointerId, pointer);
  sendRemote({ type: "touch", phase: "down", id: pointer.id, x: point.x, y: point.y });
}

function handlePointerMove(event) {
  const pointer = activePointers.get(event.pointerId);
  if (!pointer) return;
  event.preventDefault();
  const point = canvasCoordinates(event, true);
  if (!point) return;
  pointer.x = point.x;
  pointer.y = point.y;
  if (Math.hypot(event.clientX - pointer.startX, event.clientY - pointer.startY) > 9) {
    pointer.moved = true;
  }
  sendRemote({ type: "touch", phase: "move", id: pointer.id, x: point.x, y: point.y });
}

function finishPointer(event, canceled) {
  const pointer = activePointers.get(event.pointerId);
  if (!pointer) return;
  event.preventDefault();
  const point = canvasCoordinates(event, true);
  if (point) {
    pointer.x = point.x;
    pointer.y = point.y;
  }
  sendRemote({
    type: "touch",
    phase: canceled ? "cancel" : "up",
    id: pointer.id,
    x: pointer.x,
    y: pointer.y,
  });
  activePointers.delete(event.pointerId);
  if (!canceled && !pointer.moved) armRemoteKeyboard();
}

function connect() {
  clearTimeout(reconnectTimer);
  if (socket && (socket.readyState === WebSocket.CONNECTING || socket.readyState === WebSocket.OPEN)) {
    socket.onclose = null;
    socket.close();
  }
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  const currentSocket = new WebSocket(`${scheme}://${location.host}/ws?token=${encodeURIComponent(token)}&format=${streamFormat}`);
  socket = currentSocket;
  currentSocket.binaryType = "arraybuffer";
  let opened = false;
  currentSocket.onopen = () => {
    if (socket !== currentSocket) return;
    opened = true;
    reconnectAttempts = 0;
    login.hidden = true;
    viewer.hidden = false;
    showStatus(streamFormat === "jpeg" ? "Warte auf den HTTP-Kompatibilitätsstream …" : "Warte auf den ersten Frame …");
  };
  currentSocket.onmessage = (event) => {
    if (socket !== currentSocket) return;
    if (typeof event.data === "string") {
      try {
        handleControlMessage(JSON.parse(event.data));
      } catch (error) {
        console.warn("Invalid control message", error);
      }
      return;
    }
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
        clearVideoFrames();
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
  currentSocket.onclose = (event) => {
    if (socket !== currentSocket) return;
    resetMediaClock();
    setControlEnabled(false);
    if (decoder && decoder.state !== "closed") decoder.close();
    decoder = null;
    waitingForKeyframe = true;
    if (!opened) reconnectAttempts += 1;

    const shouldRefreshToken = event.code === 1008 || (reconnectAttempts >= 3 && reconnectAttempts % 3 === 0);
    if (shouldRefreshToken && rememberedPin) {
      void requestToken(rememberedPin)
        .then(() => {
          if (socket !== currentSocket) return;
          reconnectAttempts = 0;
          connect();
        })
        .catch(() => {
          // The server may still be offline. The scheduled reconnect keeps running.
        });
    } else if (shouldRefreshToken && !rememberedPin) {
      login.hidden = false;
      loginError.textContent = "Verbindung unterbrochen. Erneut anmelden oder auf die automatische Wiederverbindung warten.";
    }

    const delay = reconnectAttempts === 0
      ? 0
      : Math.min(2_000, 200 * Math.pow(1.35, Math.min(reconnectAttempts - 1, 10)));
    showStatus(`Verbindung unterbrochen – neuer Versuch${delay > 0 ? ` in ${Math.ceil(delay)} ms` : " sofort"} …`);
    reconnectTimer = setTimeout(connect, delay);
  };
  currentSocket.onerror = () => currentSocket.close();
}

async function requestToken(pin) {
  const response = await fetch("/api/login", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ pin }),
  });
  if (!response.ok) throw new Error("invalid credentials");
  const payload = await response.json();
  token = payload.token;
  rememberedPin = pin;
  sessionStorage.setItem("tesla-screen-token", token);
}

loginForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  void ensureAudioContext();
  loginError.textContent = "";
  try {
    await requestToken(pinInput.value);
    login.hidden = true;
    viewer.hidden = false;
    reconnectAttempts = 0;
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

canvas.addEventListener("pointerdown", handlePointerDown, { passive: false });
canvas.addEventListener("pointermove", handlePointerMove, { passive: false });
canvas.addEventListener("pointerup", (event) => finishPointer(event, false), { passive: false });
canvas.addEventListener("pointercancel", (event) => finishPointer(event, true), { passive: false });
canvas.addEventListener("lostpointercapture", (event) => {
  if (activePointers.has(event.pointerId)) finishPointer(event, true);
});
canvas.addEventListener("contextmenu", (event) => event.preventDefault());
canvas.addEventListener("dragstart", (event) => event.preventDefault());
canvas.addEventListener("selectstart", (event) => event.preventDefault());

setInterval(() => {
  for (const pointer of activePointers.values()) {
    sendRemote({
      type: "touch",
      phase: "move",
      id: pointer.id,
      x: pointer.x,
      y: pointer.y,
    });
  }
}, 100);

remoteKeyboard.addEventListener("beforeinput", (event) => {
  if (!controlEnabled || (!keyboardCandidate && !keyboardConfirmed)) return;
  const deleteKeys = {
    deleteContentBackward: "backspace",
    deleteWordBackward: "backspace",
    deleteSoftLineBackward: "backspace",
    deleteHardLineBackward: "backspace",
    deleteContentForward: "delete",
    deleteWordForward: "delete",
    deleteSoftLineForward: "delete",
    deleteHardLineForward: "delete",
  };
  const deleteKey = deleteKeys[event.inputType];
  if (deleteKey) {
    event.preventDefault();
    sendRemote({ type: "key", key: deleteKey });
    resetKeyboardBuffer();
    return;
  }
  if (event.inputType === "insertLineBreak" || event.inputType === "insertParagraph") {
    event.preventDefault();
    sendRemote({ type: "key", key: "enter" });
    resetKeyboardBuffer();
  }
});

remoteKeyboard.addEventListener("input", (event) => {
  if (!controlEnabled || composingText || event.isComposing) return;
  if (ignoreNextKeyboardInput) {
    ignoreNextKeyboardInput = false;
    resetKeyboardBuffer();
    return;
  }
  const text = remoteKeyboard.value.split(KEYBOARD_SENTINEL).join("");
  if (text) sendRemote({ type: "text", text });
  resetKeyboardBuffer();
});

remoteKeyboard.addEventListener("compositionstart", () => {
  composingText = true;
});

remoteKeyboard.addEventListener("compositionend", (event) => {
  composingText = false;
  if (event.data) sendRemote({ type: "text", text: event.data });
  ignoreNextKeyboardInput = true;
  resetKeyboardBuffer();
  setTimeout(() => {
    ignoreNextKeyboardInput = false;
  }, 0);
});

remoteKeyboard.addEventListener("keydown", (event) => {
  const keys = {
    Backspace: "backspace",
    Delete: "delete",
    Enter: "enter",
    Tab: "tab",
    Escape: "escape",
    ArrowLeft: "arrow_left",
    ArrowRight: "arrow_right",
    ArrowUp: "arrow_up",
    ArrowDown: "arrow_down",
    Home: "home",
    End: "end",
  };
  const key = keys[event.key];
  if (!key) return;
  event.preventDefault();
  sendRemote({ type: "key", key });
  resetKeyboardBuffer();
});

window.addEventListener("resize", updateKeyboardShift);
window.visualViewport?.addEventListener("resize", updateKeyboardShift);
window.visualViewport?.addEventListener("scroll", updateKeyboardShift);

if (!hasWebCodecs) {
  modeNote.hidden = false;
  modeNote.textContent = "HTTP-Kompatibilitätsmodus aktiv. Der Stream funktioniert ohne WebCodecs und ohne HTTPS-Zertifikat.";
}

if (token) {
  login.hidden = true;
  viewer.hidden = false;
  connect();
}
