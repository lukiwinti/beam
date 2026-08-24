use anyhow::{Context, Result};
use windows_capture::monitor::Monitor;

#[derive(Debug, Clone)]
pub struct DisplayInfo {
    pub index: usize,
    pub name: String,
    pub device_name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
}

impl DisplayInfo {
    pub fn label(&self) -> String {
        format!(
            "{} — {} × {} @ {} Hz ({})",
            self.name, self.width, self.height, self.refresh_rate, self.device_name
        )
    }
}

pub fn enumerate_displays() -> Result<Vec<DisplayInfo>> {
    Monitor::enumerate()
        .context("Windows konnte die aktiven Bildschirme nicht ermitteln")?
        .into_iter()
        .enumerate()
        .map(|(position, monitor)| {
            // `Monitor::from_index` addresses the enumeration order, which is not
            // necessarily the numeric suffix in `\\.\DISPLAYn`.
            let index = position + 1;
            Ok(DisplayInfo {
                index,
                name: monitor
                    .name()
                    .unwrap_or_else(|_| format!("Bildschirm {index}")),
                device_name: monitor
                    .device_name()
                    .unwrap_or_else(|_| format!("DISPLAY{index}")),
                width: monitor
                    .width()
                    .context("Bildschirmbreite konnte nicht gelesen werden")?,
                height: monitor
                    .height()
                    .context("Bildschirmhöhe konnte nicht gelesen werden")?,
                refresh_rate: monitor.refresh_rate().unwrap_or(0),
            })
        })
        .collect()
}
