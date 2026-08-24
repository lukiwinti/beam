use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

pub const DEFAULT_PORT: u16 = 8080;
pub const DEFAULT_FPS: u32 = 30;
pub const DEFAULT_BITRATE_KBPS: u32 = 8_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SenderConfig {
    pub monitor_index: usize,
    pub bind_address: IpAddr,
    pub port: u16,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub pin: String,
    pub capture_cursor: bool,
}

impl Default for SenderConfig {
    fn default() -> Self {
        Self {
            monitor_index: 1,
            bind_address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: DEFAULT_PORT,
            fps: DEFAULT_FPS,
            bitrate_kbps: DEFAULT_BITRATE_KBPS,
            pin: "123456".to_owned(),
            capture_cursor: true,
        }
    }
}

impl SenderConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.monitor_index == 0 {
            return Err("A monitor must be selected".to_owned());
        }
        if self.port == 0 {
            return Err("Port must be between 1 and 65535".to_owned());
        }
        if !(1..=60).contains(&self.fps) {
            return Err("Frame rate must be between 1 and 60 FPS".to_owned());
        }
        if !(500..=50_000).contains(&self.bitrate_kbps) {
            return Err("Bitrate must be between 500 and 50000 kbit/s".to_owned());
        }
        if self.pin.len() < 4 || self.pin.len() > 64 {
            return Err("PIN must contain between 4 and 64 characters".to_owned());
        }
        Ok(())
    }

    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.bind_address, self.port)
    }

    pub fn browser_url(&self, local_ip: IpAddr) -> String {
        let host = match local_ip {
            IpAddr::V4(ip) => ip.to_string(),
            IpAddr::V6(ip) => format!("[{ip}]"),
        };
        format!("http://{host}:{}", self.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        assert!(SenderConfig::default().validate().is_ok());
    }

    #[test]
    fn rejects_invalid_monitor_and_pin() {
        let mut config = SenderConfig {
            monitor_index: 0,
            ..SenderConfig::default()
        };
        assert_eq!(config.validate().unwrap_err(), "A monitor must be selected");

        config.monitor_index = 1;
        config.pin = "123".to_owned();
        assert!(config.validate().unwrap_err().contains("PIN"));
    }

    #[test]
    fn rejects_out_of_range_stream_settings() {
        let mut config = SenderConfig {
            fps: 0,
            ..SenderConfig::default()
        };
        assert!(config.validate().unwrap_err().contains("Frame rate"));

        config.fps = 30;
        config.bitrate_kbps = 100;
        assert!(config.validate().unwrap_err().contains("Bitrate"));
    }

    #[test]
    fn formats_ipv4_and_ipv6_browser_urls() {
        let config = SenderConfig::default();
        assert_eq!(
            config.browser_url("192.0.2.20".parse().unwrap()),
            "http://192.0.2.20:8080"
        );
        assert_eq!(
            config.browser_url("2001:db8::20".parse().unwrap()),
            "http://[2001:db8::20]:8080"
        );
    }
}
