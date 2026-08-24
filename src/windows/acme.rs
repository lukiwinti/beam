use crate::config::app_data_dir;
use anyhow::{Context as _, Result};
use axum_server::tls_rustls::RustlsConfig;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{Cursor, Read as _},
    os::windows::process::CommandExt as _,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
    thread,
    time::Duration,
};

const LEGO_VERSION: &str = "5.4.0";
const LEGO_ARCHIVE_URL: &str =
    "https://github.com/go-acme/lego/releases/download/v5.4.0/lego_v5.4.0_windows_amd64.zip";
const LEGO_ARCHIVE_SHA256: &str =
    "d8d7a612d5776ee77f4aba219a732e5b2bca1ecd208f3e3aaa21a4b21ca1bd08";
const LEGO_EXE_SHA256: &str = "1b510632de6a2bc5b4e14bdb55044c7c23fb4e66e9364106859c89ef7c65463c";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const RENEW_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);

#[derive(Clone, Copy)]
pub(super) struct CredentialField {
    pub(super) environment: &'static str,
    pub(super) label: &'static str,
}

#[derive(Clone, Copy)]
pub(super) struct ProviderDefinition {
    pub(super) code: &'static str,
    pub(super) name: &'static str,
    pub(super) fields: &'static [CredentialField],
}

const HETZNER_FIELDS: &[CredentialField] = &[CredentialField {
    environment: "HETZNER_API_TOKEN",
    label: "API-Token",
}];
const CLOUDFLARE_FIELDS: &[CredentialField] = &[CredentialField {
    environment: "CF_DNS_API_TOKEN",
    label: "DNS API-Token",
}];
const IONOS_FIELDS: &[CredentialField] = &[CredentialField {
    environment: "IONOS_API_KEY",
    label: "API-Key (Präfix.Secret)",
}];
const NETCUP_FIELDS: &[CredentialField] = &[
    CredentialField {
        environment: "NETCUP_CUSTOMER_NUMBER",
        label: "Kundennummer",
    },
    CredentialField {
        environment: "NETCUP_API_KEY",
        label: "API-Key",
    },
    CredentialField {
        environment: "NETCUP_API_PASSWORD",
        label: "API-Passwort",
    },
];
const DIGITALOCEAN_FIELDS: &[CredentialField] = &[CredentialField {
    environment: "DO_AUTH_TOKEN",
    label: "API-Token",
}];
const DUCKDNS_FIELDS: &[CredentialField] = &[CredentialField {
    environment: "DUCKDNS_TOKEN",
    label: "Account-Token",
}];
const DESEC_FIELDS: &[CredentialField] = &[CredentialField {
    environment: "DESEC_TOKEN",
    label: "Domain-Token",
}];
const HTTPNET_FIELDS: &[CredentialField] = &[CredentialField {
    environment: "HTTPNET_API_KEY",
    label: "API-Key",
}];
const IPV64_FIELDS: &[CredentialField] = &[CredentialField {
    environment: "IPV64_API_KEY",
    label: "API-Key",
}];
const VERCEL_FIELDS: &[CredentialField] = &[CredentialField {
    environment: "VERCEL_API_TOKEN",
    label: "API-Token",
}];

pub(super) const PROVIDERS: &[ProviderDefinition] = &[
    ProviderDefinition {
        code: "hetzner",
        name: "Hetzner",
        fields: HETZNER_FIELDS,
    },
    ProviderDefinition {
        code: "cloudflare",
        name: "Cloudflare",
        fields: CLOUDFLARE_FIELDS,
    },
    ProviderDefinition {
        code: "ionos",
        name: "IONOS",
        fields: IONOS_FIELDS,
    },
    ProviderDefinition {
        code: "netcup",
        name: "Netcup",
        fields: NETCUP_FIELDS,
    },
    ProviderDefinition {
        code: "digitalocean",
        name: "DigitalOcean",
        fields: DIGITALOCEAN_FIELDS,
    },
    ProviderDefinition {
        code: "duckdns",
        name: "Duck DNS",
        fields: DUCKDNS_FIELDS,
    },
    ProviderDefinition {
        code: "desec",
        name: "deSEC.io",
        fields: DESEC_FIELDS,
    },
    ProviderDefinition {
        code: "httpnet",
        name: "http.net",
        fields: HTTPNET_FIELDS,
    },
    ProviderDefinition {
        code: "ipv64",
        name: "IPv64",
        fields: IPV64_FIELDS,
    },
    ProviderDefinition {
        code: "vercel",
        name: "Vercel",
        fields: VERCEL_FIELDS,
    },
];

pub(super) fn provider(code: &str) -> Option<&'static ProviderDefinition> {
    PROVIDERS.iter().find(|provider| provider.code == code)
}

#[derive(Clone)]
pub(super) struct AcmeRequest {
    pub(super) domain: String,
    pub(super) email: String,
    pub(super) provider: String,
    pub(super) credentials: BTreeMap<String, String>,
}

impl AcmeRequest {
    pub(super) fn validate_credentials(&self) -> Result<()> {
        let definition = provider(&self.provider).ok_or_else(|| {
            anyhow::anyhow!(
                "Der DNS-Provider '{}' wird von dieser Version noch nicht angeboten",
                self.provider
            )
        })?;
        let missing: Vec<_> = definition
            .fields
            .iter()
            .filter(|field| {
                self.credentials
                    .get(field.environment)
                    .is_none_or(|value| value.trim().is_empty())
            })
            .map(|field| field.label)
            .collect();
        if !missing.is_empty() {
            anyhow::bail!(
                "Für {} fehlen Zugangsdaten: {}",
                definition.name,
                missing.join(", ")
            );
        }
        Ok(())
    }
}

pub(super) struct PreparedCertificate {
    pub(super) cert_path: PathBuf,
    pub(super) key_path: PathBuf,
    request: AcmeRequest,
}

impl PreparedCertificate {
    pub(super) fn start_renewer(&self, tls: RustlsConfig) -> CertificateRenewer {
        CertificateRenewer::start(self.request.clone(), tls)
    }
}

pub(super) fn prepare_certificate(request: AcmeRequest) -> Result<PreparedCertificate> {
    request.validate_credentials()?;
    run_lego(&request)?;
    let certificate_directory = acme_directory()?.join("certificates");
    let cert_path = certificate_directory.join("tesla-screen.crt");
    let key_path = certificate_directory.join("tesla-screen.key");
    if !cert_path.is_file() || !key_path.is_file() {
        anyhow::bail!(
            "Let's Encrypt hat keine Zertifikatsdateien erzeugt. Erwartet wurden {} und {}",
            cert_path.display(),
            key_path.display()
        );
    }
    Ok(PreparedCertificate {
        cert_path,
        key_path,
        request,
    })
}

pub(super) struct CertificateRenewer {
    stop_tx: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl CertificateRenewer {
    fn start(request: AcmeRequest, tls: RustlsConfig) -> Self {
        let (stop_tx, stop_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("tesla-screen-acme-renewal".to_owned())
            .spawn(move || loop {
                match stop_rx.recv_timeout(RENEW_INTERVAL) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                match run_lego(&request) {
                    Ok(()) => {
                        let certificate_directory = match acme_directory() {
                            Ok(directory) => directory.join("certificates"),
                            Err(error) => {
                                tracing::error!(%error, "ACME directory unavailable after renewal");
                                continue;
                            }
                        };
                        let cert_path = certificate_directory.join("tesla-screen.crt");
                        let key_path = certificate_directory.join("tesla-screen.key");
                        let runtime = match tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                        {
                            Ok(runtime) => runtime,
                            Err(error) => {
                                tracing::error!(%error, "TLS reload runtime could not be created");
                                continue;
                            }
                        };
                        if let Err(error) = runtime.block_on(tls.reload_from_pem_file(cert_path, key_path)) {
                            tracing::error!(%error, "renewed TLS certificate could not be loaded");
                        } else {
                            tracing::info!("Let's Encrypt certificate checked and TLS configuration reloaded");
                        }
                    }
                    Err(error) => tracing::error!(%error, "automatic Let's Encrypt renewal failed"),
                }
            })
            .ok();
        Self {
            stop_tx: Some(stop_tx),
            thread,
        }
    }

    pub(super) fn stop(mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if self
            .thread
            .as_ref()
            .is_some_and(thread::JoinHandle::is_finished)
            && let Some(thread) = self.thread.take()
        {
            let _ = thread.join();
        }
    }
}

impl Drop for CertificateRenewer {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
    }
}

fn run_lego(request: &AcmeRequest) -> Result<()> {
    let lego = ensure_lego()?;
    let acme_path = acme_directory()?;
    fs::create_dir_all(&acme_path)?;
    let credential_directory = acme_path.join(format!(
        "credentials-{}-{:016x}",
        std::process::id(),
        rand::random::<u64>()
    ));
    fs::create_dir_all(&credential_directory)?;

    let result = (|| -> Result<()> {
        let mut command = Command::new(&lego);
        command
            .creation_flags(CREATE_NO_WINDOW)
            .arg("run")
            .arg("--accept-tos")
            .arg("--email")
            .arg(request.email.trim())
            .arg("--dns")
            .arg(&request.provider)
            // lego's automatic fallback includes Cloudflare IPv6 resolvers. On
            // IPv4-only vehicle networks that makes an otherwise successful
            // DNS-01 challenge fail during the local propagation check.
            .arg("--dns.resolvers")
            .arg("1.1.1.1:53")
            .arg("--dns.resolvers")
            .arg("1.0.0.1:53")
            // Recursive resolvers can cache NXDOMAIN for the challenge name
            // before Hetzner publishes the short-lived TXT record. Validate
            // against the authoritative nameservers instead; Let's Encrypt
            // still performs its own independent public DNS validation.
            .arg("--dns.propagation.disable-rns")
            .arg("--domains")
            .arg(request.domain.trim())
            .arg("--cert.name")
            .arg("tesla-screen")
            .arg("--path")
            .arg(&acme_path)
            .arg("--renew-days")
            .arg("30")
            .arg("--no-random-sleep")
            .arg("--user-agent")
            .arg(concat!("TeslaScreenSender/", env!("CARGO_PKG_VERSION")))
            .arg("--log.format")
            .arg("text");

        for (index, (environment, value)) in request.credentials.iter().enumerate() {
            if !valid_environment_name(environment) {
                anyhow::bail!("Ungültiger lego-Variablenname: {environment}");
            }
            let value_path = credential_directory.join(format!("credential-{index}.txt"));
            fs::write(&value_path, value)?;
            command.env(format!("{environment}_FILE"), value_path);
        }

        let output = command
            .output()
            .context("Der integrierte ACME-Client konnte nicht gestartet werden")?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let details = if !stderr.is_empty() { stderr } else { stdout };
        anyhow::bail!("Let's-Encrypt-/DNS-Challenge fehlgeschlagen: {details}")
    })();

    if let Err(error) = fs::remove_dir_all(&credential_directory) {
        tracing::warn!(%error, path = %credential_directory.display(), "temporary ACME credentials could not be removed");
    }
    result
}

fn ensure_lego() -> Result<PathBuf> {
    let tools_directory = app_data_dir()?.join("tools");
    fs::create_dir_all(&tools_directory)?;
    let executable = tools_directory.join(format!("lego-{LEGO_VERSION}.exe"));
    if executable.is_file() && file_sha256(&executable)? == LEGO_EXE_SHA256 {
        return Ok(executable);
    }

    let response = reqwest::blocking::Client::builder()
        .user_agent(concat!("TeslaScreenSender/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(300))
        .build()?
        .get(LEGO_ARCHIVE_URL)
        .send()
        .context("Der ACME-Client konnte nicht von GitHub geladen werden")?
        .error_for_status()
        .context("GitHub hat den Download des ACME-Clients abgelehnt")?;
    let archive = response
        .bytes()
        .context("Der ACME-Client-Download ist abgebrochen")?;
    let archive_hash = hex_sha256(&archive);
    if archive_hash != LEGO_ARCHIVE_SHA256 {
        anyhow::bail!(
            "Sicherheitsprüfung des ACME-Downloads fehlgeschlagen (SHA256 {archive_hash})"
        );
    }

    let mut zip = zip::ZipArchive::new(Cursor::new(archive))?;
    let mut entry = zip
        .by_name("lego.exe")
        .context("Das ACME-Archiv enthält keine lego.exe")?;
    let temporary = tools_directory.join(format!("lego-{LEGO_VERSION}.part"));
    let mut bytes = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut bytes)?;
    if hex_sha256(&bytes) != LEGO_EXE_SHA256 {
        anyhow::bail!("Sicherheitsprüfung der extrahierten lego.exe fehlgeschlagen");
    }
    fs::write(&temporary, bytes)?;
    if executable.exists() {
        fs::remove_file(&executable)?;
    }
    fs::rename(&temporary, &executable)?;
    Ok(executable)
}

fn acme_directory() -> Result<PathBuf> {
    Ok(app_data_dir()?.join("acme"))
}

fn file_sha256(path: &Path) -> Result<String> {
    let bytes = fs::read(path)?;
    Ok(hex_sha256(&bytes))
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn valid_environment_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_registry_exposes_hetzner_and_validates_credentials() {
        let definition = provider("hetzner").unwrap();
        assert_eq!(definition.name, "Hetzner");
        assert_eq!(definition.fields[0].environment, "HETZNER_API_TOKEN");

        let mut request = AcmeRequest {
            domain: "screen.example.org".to_owned(),
            email: "admin@example.org".to_owned(),
            provider: "hetzner".to_owned(),
            credentials: BTreeMap::new(),
        };
        assert!(request.validate_credentials().is_err());
        request
            .credentials
            .insert("HETZNER_API_TOKEN".to_owned(), "secret".to_owned());
        request.validate_credentials().unwrap();
    }

    #[test]
    fn only_safe_environment_names_are_accepted() {
        assert!(valid_environment_name("HETZNER_API_TOKEN"));
        assert!(!valid_environment_name("Hetzner-Token"));
        assert!(!valid_environment_name("A=B"));
    }
}
