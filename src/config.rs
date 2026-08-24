use serde::{Deserialize, Serialize};
use std::{
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
};

pub const DEFAULT_PORT: u16 = 8080;
pub const DEFAULT_FPS: u32 = 30;
pub const DEFAULT_BITRATE_KBPS: u32 = 8_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SenderConfig {
    pub monitor_index: usize,
    pub bind_address: IpAddr,
    pub port: u16,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub pin: String,
    pub capture_cursor: bool,
    pub audio_enabled: bool,
    pub https_enabled: bool,
    pub https_domain: String,
    pub acme_email: String,
    pub dns_provider: String,
    pub acme_accept_tos: bool,
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
            audio_enabled: true,
            https_enabled: false,
            https_domain: String::new(),
            acme_email: String::new(),
            dns_provider: "hetzner".to_owned(),
            acme_accept_tos: false,
        }
    }
}

impl SenderConfig {
    pub fn load() -> anyhow::Result<Self> {
        Self::load_from_path(&settings_path()?)
    }

    pub fn save(&self) -> anyhow::Result<PathBuf> {
        let path = settings_path()?;
        self.save_to_path(&path)?;
        Ok(path)
    }

    fn load_from_path(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = fs::read(path)?;
        let config: Self = serde_json::from_slice(&bytes)?;
        config.validate().map_err(anyhow::Error::msg)?;
        Ok(config)
    }

    fn save_to_path(&self, path: &Path) -> anyhow::Result<()> {
        self.validate().map_err(anyhow::Error::msg)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

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
        if self.https_enabled {
            let domain = self.https_domain.trim();
            if domain.is_empty()
                || domain.len() > 253
                || domain.contains(|character: char| {
                    character.is_whitespace() || matches!(character, '/' | ':' | '*')
                })
                || !domain.contains('.')
            {
                return Err(
                    "HTTPS-Domain muss ein Hostname ohne Protokoll, Port oder Pfad sein".to_owned(),
                );
            }
            let email = self.acme_email.trim();
            if !email.contains('@') || email.starts_with('@') || email.ends_with('@') {
                return Err(
                    "Für Let's Encrypt wird eine gültige E-Mail-Adresse benötigt".to_owned(),
                );
            }
            if self.dns_provider.trim().is_empty() {
                return Err("Ein DNS-Provider muss ausgewählt werden".to_owned());
            }
            if !self.acme_accept_tos {
                return Err(
                    "Die Nutzungsbedingungen von Let's Encrypt müssen akzeptiert werden".to_owned(),
                );
            }
        }
        Ok(())
    }

    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.bind_address, self.port)
    }

    pub fn browser_url(&self, local_ip: IpAddr) -> String {
        if self.https_enabled {
            return format!("https://{}:{}", self.https_domain.trim(), self.port);
        }
        let host = match local_ip {
            IpAddr::V4(ip) => ip.to_string(),
            IpAddr::V6(ip) => format!("[{ip}]"),
        };
        format!("http://{host}:{}", self.port)
    }
}

fn settings_path() -> anyhow::Result<PathBuf> {
    Ok(app_data_dir()?.join("settings.json"))
}

pub(crate) fn app_data_dir() -> anyhow::Result<PathBuf> {
    let local_app_data = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| anyhow::anyhow!("LOCALAPPDATA ist unter Windows nicht gesetzt"))?;
    Ok(PathBuf::from(local_app_data).join("TeslaScreenSender"))
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
    fn https_requires_hostname_email_and_terms() {
        let mut config = SenderConfig {
            https_enabled: true,
            ..SenderConfig::default()
        };
        assert!(config.validate().unwrap_err().contains("HTTPS-Domain"));
        config.https_domain = "https://screen.example.org".to_owned();
        assert!(config.validate().unwrap_err().contains("HTTPS-Domain"));
        config.https_domain = "screen.example.org".to_owned();
        assert!(config.validate().unwrap_err().contains("E-Mail"));
        config.acme_email = "admin@example.org".to_owned();
        assert!(
            config
                .validate()
                .unwrap_err()
                .contains("Nutzungsbedingungen")
        );
        config.acme_accept_tos = true;
        config.validate().unwrap();
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

        let https = SenderConfig {
            https_enabled: true,
            https_domain: "screen.example.org".to_owned(),
            acme_email: "admin@example.org".to_owned(),
            acme_accept_tos: true,
            ..SenderConfig::default()
        };
        assert_eq!(
            https.browser_url("192.0.2.20".parse().unwrap()),
            "https://screen.example.org:8080"
        );
    }

    #[test]
    fn settings_round_trip_and_accept_missing_new_fields() {
        let directory = std::env::temp_dir().join(format!(
            "tesla-screen-settings-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let path = directory.join("settings.json");
        let config = SenderConfig {
            fps: 42,
            audio_enabled: false,
            ..SenderConfig::default()
        };
        config.save_to_path(&path).unwrap();
        assert_eq!(SenderConfig::load_from_path(&path).unwrap(), config);

        fs::write(
            &path,
            br#"{"monitor_index":1,"bind_address":"0.0.0.0","port":8080,"fps":30,"bitrate_kbps":8000,"pin":"123456","capture_cursor":true}"#,
        )
        .unwrap();
        assert!(SenderConfig::load_from_path(&path).unwrap().audio_enabled);
        fs::remove_dir_all(directory).unwrap();
    }
}
