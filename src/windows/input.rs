use crate::server::{
    ControlSignal, NormalizedRect, RemoteInputEvent, RemoteKey, StreamState, TouchPhase,
};
use anyhow::{Context as _, Result, anyhow, bail};
use std::{
    collections::{HashMap, HashSet},
    mem::size_of,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{POINT, RECT},
        Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO},
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize,
        },
        UI::{
            Accessibility::{
                CUIAutomation, IUIAutomation, IUIAutomationValuePattern, UIA_EditControlTypeId,
                UIA_ValuePatternId,
            },
            Input::{
                KeyboardAndMouse::{
                    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
                    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, SendInput, VIRTUAL_KEY, VK_BACK, VK_DELETE,
                    VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_RETURN, VK_RIGHT, VK_TAB,
                    VK_UP,
                },
                Pointer::{
                    InitializeTouchInjection, InjectTouchInput, POINTER_FLAG_CANCELED,
                    POINTER_FLAG_CONFIDENCE, POINTER_FLAG_DOWN, POINTER_FLAG_INCONTACT,
                    POINTER_FLAG_INRANGE, POINTER_FLAG_PRIMARY, POINTER_FLAG_UP,
                    POINTER_FLAG_UPDATE, POINTER_FLAGS, POINTER_TOUCH_INFO, TOUCH_FEEDBACK_NONE,
                },
            },
            WindowsAndMessaging::{
                PT_TOUCH, TOUCH_MASK_CONTACTAREA, TOUCH_MASK_ORIENTATION, TOUCH_MASK_PRESSURE,
            },
        },
    },
    core::Error as WindowsError,
};
use windows_capture::monitor::Monitor;

const MAX_TOUCH_CONTACTS: u32 = 10;
const FOCUS_CHECK_DELAY: Duration = Duration::from_millis(90);
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub(super) struct InputSession {
    state: Arc<StreamState>,
    stop_tx: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl InputSession {
    pub(super) fn start(state: Arc<StreamState>, monitor_index: usize) -> Result<Self> {
        let (input_tx, input_rx) = mpsc::channel();
        let (stop_tx, stop_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker_state = Arc::clone(&state);
        let thread = thread::Builder::new()
            .name("tesla-screen-input".to_owned())
            .spawn(move || input_thread(worker_state, monitor_index, input_rx, stop_rx, ready_tx))
            .context("Windows-Bedienungs-Thread konnte nicht gestartet werden")?;

        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {
                state.enable_control(input_tx);
                Ok(Self {
                    state,
                    stop_tx: Some(stop_tx),
                    thread: Some(thread),
                })
            }
            Ok(Err(error)) => {
                let _ = thread.join();
                bail!(error)
            }
            Err(_) => {
                let _ = stop_tx.send(());
                let _ = thread.join();
                bail!("Windows-Bedienung hat nicht rechtzeitig geantwortet")
            }
        }
    }

    pub(super) fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        self.state.disable_control();
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for InputSession {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

fn input_thread(
    state: Arc<StreamState>,
    monitor_index: usize,
    input_rx: mpsc::Receiver<RemoteInputEvent>,
    stop_rx: mpsc::Receiver<()>,
    ready_tx: mpsc::SyncSender<Result<(), String>>,
) {
    let setup = (|| -> Result<_> {
        unsafe {
            InitializeTouchInjection(MAX_TOUCH_CONTACTS, TOUCH_FEEDBACK_NONE)
                .context("Windows-Touchinjektion konnte nicht initialisiert werden")?;
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .context("COM konnte für die Textfelderkennung nicht initialisiert werden")?;
        }
        let initialized = (|| -> Result<_> {
            let automation: IUIAutomation = unsafe {
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
                    .context("Windows UI Automation konnte nicht gestartet werden")?
            };
            let injector = TouchInjector::new(monitor_index)?;
            Ok((injector, automation))
        })();
        if initialized.is_err() {
            unsafe { CoUninitialize() };
        }
        initialized
    })();

    let (mut injector, automation) = match setup {
        Ok(setup) => {
            let _ = ready_tx.send(Ok(()));
            setup
        }
        Err(error) => {
            let _ = ready_tx.send(Err(format!(
                "Bedienung konnte nicht gestartet werden: {error}"
            )));
            return;
        }
    };

    let mut focus_check_at = None;
    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }
        let timeout = focus_check_at
            .map(|deadline: Instant| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(IDLE_POLL_INTERVAL)
            .min(IDLE_POLL_INTERVAL);

        match input_rx.recv_timeout(timeout) {
            Ok(event) => {
                let schedule_focus_check = matches!(
                    event,
                    RemoteInputEvent::Touch {
                        phase: TouchPhase::Up,
                        ..
                    } | RemoteInputEvent::Key {
                        key: RemoteKey::Enter | RemoteKey::Tab | RemoteKey::Escape,
                    }
                );
                if let Err(error) = handle_input(&mut injector, &event) {
                    tracing::warn!(%error, "remote Windows input could not be injected");
                }
                if schedule_focus_check {
                    focus_check_at = Some(Instant::now() + FOCUS_CHECK_DELAY);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if focus_check_at.is_some_and(|deadline| Instant::now() >= deadline) {
            let rect = focused_edit_rect(&automation, injector.monitor_rect())
                .ok()
                .flatten();
            state.publish_control(ControlSignal::Keyboard {
                show: rect.is_some(),
                rect,
            });
            focus_check_at = None;
        }
    }

    injector.cancel_all();
    unsafe { CoUninitialize() };
}

fn handle_input(injector: &mut TouchInjector, event: &RemoteInputEvent) -> Result<()> {
    match event {
        RemoteInputEvent::Touch { phase, id, x, y } => injector.touch(*phase, *id, *x, *y),
        RemoteInputEvent::Text { text } => send_unicode_text(text),
        RemoteInputEvent::Key { key } => send_virtual_key(*key),
        RemoteInputEvent::CancelAll => {
            injector.cancel_all();
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Contact {
    pointer_id: u32,
    point: POINT,
    primary: bool,
}

struct TouchInjector {
    monitor_rect: RECT,
    contacts: HashMap<u32, Contact>,
}

impl TouchInjector {
    fn new(monitor_index: usize) -> Result<Self> {
        let monitor = Monitor::from_index(monitor_index)
            .with_context(|| format!("Bildschirm {monitor_index} wurde nicht gefunden"))?;
        let mut info = MONITORINFO {
            cbSize: u32::try_from(size_of::<MONITORINFO>()).expect("MONITORINFO size fits u32"),
            ..MONITORINFO::default()
        };
        unsafe {
            GetMonitorInfoW(HMONITOR(monitor.as_raw_hmonitor()), &mut info)
                .ok()
                .context("Bildschirmposition konnte nicht gelesen werden")?;
        }
        Ok(Self {
            monitor_rect: info.rcMonitor,
            contacts: HashMap::new(),
        })
    }

    const fn monitor_rect(&self) -> RECT {
        self.monitor_rect
    }

    fn touch(&mut self, phase: TouchPhase, remote_id: u32, x: f64, y: f64) -> Result<()> {
        match phase {
            TouchPhase::Down => self.down(remote_id, x, y),
            TouchPhase::Move => self.update(remote_id, x, y),
            TouchPhase::Up => self.up(remote_id, false),
            TouchPhase::Cancel => self.up(remote_id, true),
        }
    }

    fn down(&mut self, remote_id: u32, x: f64, y: f64) -> Result<()> {
        if self.contacts.contains_key(&remote_id) {
            self.up(remote_id, true)?;
        }
        if self.contacts.len() >= MAX_TOUCH_CONTACTS as usize {
            bail!("Zu viele gleichzeitige Touchkontakte")
        }
        let used: HashSet<u32> = self
            .contacts
            .values()
            .map(|contact| contact.pointer_id)
            .collect();
        let pointer_id = (1..=MAX_TOUCH_CONTACTS)
            .find(|id| !used.contains(id))
            .ok_or_else(|| anyhow!("Kein freier Windows-Touchkontakt"))?;
        let point = self.map_point(x, y);
        let primary = self.contacts.is_empty();
        let mut flags = POINTER_FLAG_INRANGE
            | POINTER_FLAG_INCONTACT
            | POINTER_FLAG_DOWN
            | POINTER_FLAG_CONFIDENCE;
        if primary {
            flags |= POINTER_FLAG_PRIMARY;
        }
        let contact = Contact {
            pointer_id,
            point,
            primary,
        };
        let mut frame = self.update_frame(None);
        frame.push(touch_info(contact, flags));
        inject_contacts(&frame)?;
        self.contacts.insert(remote_id, contact);
        Ok(())
    }

    fn update(&mut self, remote_id: u32, x: f64, y: f64) -> Result<()> {
        let point = self.map_point(x, y);
        let Some(contact) = self.contacts.get(&remote_id).copied() else {
            return Ok(());
        };
        let updated = Contact { point, ..contact };
        let mut frame = self.update_frame(Some(remote_id));
        frame.push(touch_info(updated, update_flags(updated.primary)));
        inject_contacts(&frame)?;
        self.contacts.insert(remote_id, updated);
        Ok(())
    }

    fn up(&mut self, remote_id: u32, canceled: bool) -> Result<()> {
        let Some(contact) = self.contacts.get(&remote_id).copied() else {
            return Ok(());
        };
        let mut flags = POINTER_FLAG_INRANGE | POINTER_FLAG_UP;
        if contact.primary {
            flags |= POINTER_FLAG_PRIMARY;
        }
        if canceled {
            flags |= POINTER_FLAG_CANCELED;
        }
        let mut frame = self.update_frame(Some(remote_id));
        frame.push(touch_info(contact, flags));
        inject_contacts(&frame)?;
        self.contacts.remove(&remote_id);
        Ok(())
    }

    fn cancel_all(&mut self) {
        let contacts = std::mem::take(&mut self.contacts);
        let frame: Vec<_> = contacts
            .into_values()
            .map(|contact| {
                let mut flags = POINTER_FLAG_INRANGE | POINTER_FLAG_UP | POINTER_FLAG_CANCELED;
                if contact.primary {
                    flags |= POINTER_FLAG_PRIMARY;
                }
                touch_info(contact, flags)
            })
            .collect();
        let _ = inject_contacts(&frame);
    }

    fn update_frame(&self, except_remote_id: Option<u32>) -> Vec<POINTER_TOUCH_INFO> {
        self.contacts
            .iter()
            .filter(|(remote_id, _)| Some(**remote_id) != except_remote_id)
            .map(|(_, contact)| touch_info(*contact, update_flags(contact.primary)))
            .collect()
    }

    fn map_point(&self, x: f64, y: f64) -> POINT {
        let width = (self.monitor_rect.right - self.monitor_rect.left).max(1);
        let height = (self.monitor_rect.bottom - self.monitor_rect.top).max(1);
        POINT {
            x: self.monitor_rect.left + (x.clamp(0.0, 1.0) * f64::from(width - 1)).round() as i32,
            y: self.monitor_rect.top + (y.clamp(0.0, 1.0) * f64::from(height - 1)).round() as i32,
        }
    }
}

fn update_flags(primary: bool) -> POINTER_FLAGS {
    let mut flags = POINTER_FLAG_INRANGE
        | POINTER_FLAG_INCONTACT
        | POINTER_FLAG_UPDATE
        | POINTER_FLAG_CONFIDENCE;
    if primary {
        flags |= POINTER_FLAG_PRIMARY;
    }
    flags
}

fn touch_info(contact: Contact, flags: POINTER_FLAGS) -> POINTER_TOUCH_INFO {
    let radius = 3;
    POINTER_TOUCH_INFO {
        pointerInfo: windows::Win32::UI::Input::Pointer::POINTER_INFO {
            pointerType: PT_TOUCH,
            pointerId: contact.pointer_id,
            pointerFlags: flags,
            ptPixelLocation: contact.point,
            ..Default::default()
        },
        touchMask: TOUCH_MASK_CONTACTAREA | TOUCH_MASK_ORIENTATION | TOUCH_MASK_PRESSURE,
        rcContact: RECT {
            left: contact.point.x - radius,
            top: contact.point.y - radius,
            right: contact.point.x + radius,
            bottom: contact.point.y + radius,
        },
        orientation: 90,
        pressure: 32_000,
        ..Default::default()
    }
}

fn inject_contacts(contacts: &[POINTER_TOUCH_INFO]) -> Result<()> {
    if contacts.is_empty() {
        return Ok(());
    }
    unsafe { InjectTouchInput(contacts) }.context("Windows hat die Toucheingabe abgelehnt")
}

fn send_unicode_text(text: &str) -> Result<()> {
    let mut inputs = Vec::with_capacity(text.encode_utf16().count() * 2);
    for unit in text.encode_utf16() {
        inputs.push(keyboard_input(VIRTUAL_KEY(0), unit, KEYEVENTF_UNICODE));
        inputs.push(keyboard_input(
            VIRTUAL_KEY(0),
            unit,
            KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
        ));
    }
    send_inputs(&inputs)
}

fn send_virtual_key(key: RemoteKey) -> Result<()> {
    let extended = matches!(
        key,
        RemoteKey::Delete
            | RemoteKey::ArrowLeft
            | RemoteKey::ArrowRight
            | RemoteKey::ArrowUp
            | RemoteKey::ArrowDown
            | RemoteKey::Home
            | RemoteKey::End
    );
    let virtual_key = match key {
        RemoteKey::Backspace => VK_BACK,
        RemoteKey::Delete => VK_DELETE,
        RemoteKey::Enter => VK_RETURN,
        RemoteKey::Tab => VK_TAB,
        RemoteKey::Escape => VK_ESCAPE,
        RemoteKey::ArrowLeft => VK_LEFT,
        RemoteKey::ArrowRight => VK_RIGHT,
        RemoteKey::ArrowUp => VK_UP,
        RemoteKey::ArrowDown => VK_DOWN,
        RemoteKey::Home => VK_HOME,
        RemoteKey::End => VK_END,
    };
    let down_flags = if extended {
        KEYEVENTF_EXTENDEDKEY
    } else {
        Default::default()
    };
    send_inputs(&[
        keyboard_input(virtual_key, 0, down_flags),
        keyboard_input(virtual_key, 0, down_flags | KEYEVENTF_KEYUP),
    ])
}

fn keyboard_input(
    virtual_key: VIRTUAL_KEY,
    scan_code: u16,
    flags: windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS,
) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: virtual_key,
                wScan: scan_code,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send_inputs(inputs: &[INPUT]) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) } as usize;
    if sent != inputs.len() {
        return Err(WindowsError::from_thread())
            .context("Windows hat die Tastatureingabe abgelehnt");
    }
    Ok(())
}

fn focused_edit_rect(automation: &IUIAutomation, monitor: RECT) -> Result<Option<NormalizedRect>> {
    let element = unsafe { automation.GetFocusedElement() }
        .context("Fokussiertes Windows-Element konnte nicht gelesen werden")?;
    if unsafe { element.CurrentControlType() }? != UIA_EditControlTypeId
        || !unsafe { element.CurrentIsEnabled() }?.as_bool()
    {
        return Ok(None);
    }

    if let Ok(value_pattern) =
        unsafe { element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) }
        && unsafe { value_pattern.CurrentIsReadOnly() }?.as_bool()
    {
        return Ok(None);
    }

    let bounds = unsafe { element.CurrentBoundingRectangle() }?;
    let width = f64::from((monitor.right - monitor.left).max(1));
    let height = f64::from((monitor.bottom - monitor.top).max(1));
    let left = f64::from(bounds.left.max(monitor.left) - monitor.left) / width;
    let top = f64::from(bounds.top.max(monitor.top) - monitor.top) / height;
    let right = f64::from(bounds.right.min(monitor.right) - monitor.left) / width;
    let bottom = f64::from(bounds.bottom.min(monitor.bottom) - monitor.top) / height;
    if right <= left || bottom <= top {
        return Ok(None);
    }
    Ok(Some(NormalizedRect {
        left: left.clamp(0.0, 1.0),
        top: top.clamp(0.0, 1.0),
        right: right.clamp(0.0, 1.0),
        bottom: bottom.clamp(0.0, 1.0),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_points_map_across_negative_monitor_coordinates() {
        let injector = TouchInjector {
            monitor_rect: RECT {
                left: -1920,
                top: 100,
                right: 0,
                bottom: 1180,
            },
            contacts: HashMap::new(),
        };
        assert_eq!(injector.map_point(0.0, 0.0), POINT { x: -1920, y: 100 });
        assert_eq!(injector.map_point(1.0, 1.0), POINT { x: -1, y: 1179 });
    }
}
