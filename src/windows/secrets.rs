use crate::config::app_data_dir;
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use windows::Win32::{
    Foundation::{HLOCAL, LocalFree},
    Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    },
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct SecretStore {
    providers: BTreeMap<String, BTreeMap<String, String>>,
}

impl SecretStore {
    pub(super) fn load() -> Result<Self> {
        Self::load_from_path(&secret_path()?)
    }

    pub(super) fn save(&self) -> Result<PathBuf> {
        let path = secret_path()?;
        self.save_to_path(&path)?;
        Ok(path)
    }

    pub(super) fn credentials(&self, provider: &str) -> BTreeMap<String, String> {
        self.providers.get(provider).cloned().unwrap_or_default()
    }

    pub(super) fn credentials_mut(&mut self, provider: &str) -> &mut BTreeMap<String, String> {
        self.providers.entry(provider.to_owned()).or_default()
    }

    fn load_from_path(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let protected = fs::read(path).with_context(|| {
            format!(
                "ACME-Zugangsdaten konnten nicht gelesen werden: {}",
                path.display()
            )
        })?;
        let plaintext = unprotect(&protected)
            .context("ACME-Zugangsdaten konnten nicht entschlüsselt werden")?;
        serde_json::from_slice(&plaintext).context("ACME-Zugangsdaten sind beschädigt")
    }

    fn save_to_path(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let plaintext = serde_json::to_vec(self)?;
        let protected =
            protect(&plaintext).context("ACME-Zugangsdaten konnten nicht verschlüsselt werden")?;
        fs::write(path, protected).with_context(|| {
            format!(
                "ACME-Zugangsdaten konnten nicht gespeichert werden: {}",
                path.display()
            )
        })
    }
}

fn secret_path() -> Result<PathBuf> {
    Ok(app_data_dir()?.join("acme-credentials.dpapi"))
}

fn protect(plaintext: &[u8]) -> Result<Vec<u8>> {
    let input_length = u32::try_from(plaintext.len()).context("Zugangsdaten sind zu groß")?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: input_length,
        pbData: plaintext.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: Both DATA_BLOB values refer to valid buffers for the duration of the call.
    unsafe {
        CryptProtectData(
            &input,
            windows::core::w!("Tesla Screen Sender ACME credentials"),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )?;
        copy_and_free(output)
    }
}

fn unprotect(protected: &[u8]) -> Result<Vec<u8>> {
    let input_length = u32::try_from(protected.len()).context("Zugangsdaten sind zu groß")?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: input_length,
        pbData: protected.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: Both DATA_BLOB values refer to valid buffers for the duration of the call.
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )?;
        copy_and_free(output)
    }
}

unsafe fn copy_and_free(output: CRYPT_INTEGER_BLOB) -> Result<Vec<u8>> {
    if output.pbData.is_null() {
        anyhow::bail!("Windows DPAPI hat keinen Ausgabepuffer geliefert");
    }
    // SAFETY: DPAPI returned cbData readable bytes at pbData.
    let bytes =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    // SAFETY: DPAPI allocates the returned buffer with LocalAlloc.
    let remaining = unsafe { LocalFree(Some(HLOCAL(output.pbData.cast()))) };
    if !remaining.is_invalid() {
        anyhow::bail!("Windows DPAPI-Ausgabepuffer konnte nicht freigegeben werden");
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpapi_round_trip_is_not_plaintext() {
        let plaintext = b"HETZNER_API_TOKEN=secret-test-value";
        let protected = protect(plaintext).unwrap();
        assert_ne!(protected, plaintext);
        assert_eq!(unprotect(&protected).unwrap(), plaintext);
    }
}
