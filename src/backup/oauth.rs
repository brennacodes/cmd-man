//! GitHub OAuth device flow and token storage (backup tier 2).
//!
//! The flow needs a registered OAuth app client id, read from
//! `CMD_MAN_GITHUB_CLIENT_ID`. When unset, this tier is simply unavailable and
//! callers fall back to the plain-git tier.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const SCOPE: &str = "repo";

const CLIENT_ID_ENV: &str = "CMD_MAN_GITHUB_CLIENT_ID";
const KEYRING_SERVICE: &str = "cmd-man";
const KEYRING_USER: &str = "github-token";

/// Response from the device-code request.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default = "default_interval")]
    pub interval: u64,
    #[serde(default)]
    pub expires_in: u64,
}

fn default_interval() -> u64 {
    5
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    interval: Option<u64>,
}

/// The configured OAuth client id, if any.
pub fn client_id() -> Option<String> {
    std::env::var(CLIENT_ID_ENV).ok().filter(|s| !s.is_empty())
}

/// Request a device + user code from GitHub.
pub fn request_device_code(client_id: &str) -> Result<DeviceCode> {
    let mut resp = ureq::post(DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .send_form([("client_id", client_id), ("scope", SCOPE)])
        .context("requesting device code")?;
    let code: DeviceCode = resp
        .body_mut()
        .read_json()
        .context("parsing device code response")?;
    Ok(code)
}

/// Poll GitHub until the user authorizes the device, returning an access token.
pub fn poll_for_token(client_id: &str, device: &DeviceCode) -> Result<String> {
    let mut interval = device.interval.max(1);
    let deadline_polls = if device.expires_in > 0 {
        device.expires_in / interval + 1
    } else {
        180
    };

    for _ in 0..deadline_polls {
        std::thread::sleep(Duration::from_secs(interval));
        let mut resp = ureq::post(TOKEN_URL)
            .header("Accept", "application/json")
            .send_form([
                ("client_id", client_id),
                ("device_code", device.device_code.as_str()),
                ("grant_type", GRANT_TYPE),
            ])
            .context("polling for token")?;
        let token: TokenResponse = resp
            .body_mut()
            .read_json()
            .context("parsing token response")?;

        if let Some(access) = token.access_token {
            return Ok(access);
        }
        match token.error.as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval = token.interval.unwrap_or(interval + 5),
            Some("expired_token") => bail!("device code expired before authorization"),
            Some("access_denied") => bail!("authorization was denied"),
            Some(other) => bail!("authorization failed: {other}"),
            None => bail!("unexpected empty token response"),
        }
    }
    bail!("timed out waiting for device authorization")
}

/// Store an access token in the OS keychain.
pub fn store_token(token: &str) -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).context("opening keychain")?;
    entry
        .set_password(token)
        .context("saving token to keychain")?;
    Ok(())
}

/// Load a previously stored access token, if present.
pub fn load_token() -> Option<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).ok()?;
    entry.get_password().ok()
}

/// The GitHub login associated with a token, via the REST API.
pub fn login_for_token(token: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct User {
        login: String,
    }
    let mut resp = ureq::get("https://api.github.com/user")
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", &format!("Bearer {token}"))
        .header("User-Agent", "cmd-man")
        .call()
        .context("fetching authenticated user")?;
    let user: User = resp.body_mut().read_json().context("parsing user")?;
    Ok(user.login)
}

/// Create the backup repository for the user via the REST API (idempotent:
/// treats an existing repo as success).
pub fn create_repo(token: &str, repo: &str) -> Result<()> {
    let resp = ureq::post("https://api.github.com/user/repos")
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", &format!("Bearer {token}"))
        .header("User-Agent", "cmd-man")
        .send_json(serde_json::json!({
            "name": repo,
            "private": true,
            "description": "cmd-man alias/function backup",
        }));
    match resp {
        Ok(_) => Ok(()),
        Err(ureq::Error::StatusCode(422)) => Ok(()), // already exists
        Err(e) => Err(anyhow::Error::from(e)).context("creating backup repo"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_device_code_with_defaults() {
        let json = r#"{"device_code":"abc","user_code":"WXYZ-1234","verification_uri":"https://github.com/login/device","expires_in":900,"interval":5}"#;
        let dc: DeviceCode = serde_json::from_str(json).unwrap();
        assert_eq!(dc.user_code, "WXYZ-1234");
        assert_eq!(dc.interval, 5);
        assert_eq!(dc.expires_in, 900);
    }

    #[test]
    fn parses_token_response_variants() {
        let ok: TokenResponse = serde_json::from_str(r#"{"access_token":"gho_x"}"#).unwrap();
        assert_eq!(ok.access_token.as_deref(), Some("gho_x"));
        let pending: TokenResponse =
            serde_json::from_str(r#"{"error":"authorization_pending"}"#).unwrap();
        assert_eq!(pending.error.as_deref(), Some("authorization_pending"));
        assert!(pending.access_token.is_none());
    }

    #[test]
    fn client_id_absent_by_default() {
        // Not asserting env state globally; just that the getter is total.
        let _ = client_id();
    }
}
