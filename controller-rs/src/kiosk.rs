//! Kiosk Satellite login for Echo Shows that run its EchoMuse plugin (`kiosk_plugin`;
//! echo-show-companion-plan.md §11.2, device/internal/companion/kiosk.go).
//!
//! `POST /api/devices/{id}/kiosk_login {password}` logs in at the device's own Kiosk
//! Satellite remote API, `POST http://<device ip>:<kioskPort>/api/login {password,
//! ttl_days: 3650}`, which answers `{token}` (Kiosk Satellite's docs/remote-api.md,
//! "Authentication"). Only the token is kept, in `devices.kiosk_token`; the password is
//! never stored or logged. The device's runtime then sends it the token in `config`
//! (`kioskToken`), with which the firmware installs and switches on the plugin over
//! 127.0.0.1. Plain HTTP is the only transport the plugin installer supports.

use std::time::Duration;

use echomuse_api::{BoxFuture, KioskLogin, ServiceError};
use echomuse_core::types::DeviceId;
use echomuse_db::{DbHandle, WriteCommand};
use echomuse_settings::service::SettingsHandle;
use rusqlite::{Transaction, params};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::watch;

use crate::app::Directory;

/// The longest token lifetime Kiosk Satellite issues, in days.
pub const TOKEN_DAYS: u32 = 3650;
/// How long the login may take.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Stores a device's Kiosk Satellite token. False when no row has the id.
struct StoreToken {
    device: DeviceId,
    token: String,
}

impl WriteCommand for StoreToken {
    type Output = bool;
    type Error = rusqlite::Error;

    fn apply(self, tx: &Transaction<'_>) -> rusqlite::Result<bool> {
        let changed = tx
            .prepare_cached("UPDATE devices SET kiosk_token = ?2 WHERE device_id = ?1")?
            .execute(params![self.device.0, self.token])?;
        Ok(changed > 0)
    }
}

#[derive(Deserialize)]
struct LoginAnswer {
    token: String,
}

/// [`KioskLogin`] over the device's address, its `kioskPort` and its runtime.
pub struct KioskSatellite {
    pub db: DbHandle,
    pub settings: SettingsHandle,
    pub directory: watch::Receiver<Directory>,
    pub http: reqwest::Client,
}

impl KioskSatellite {
    async fn login_now(&self, device: DeviceId, password: String) -> Result<(), ServiceError> {
        let handle = self
            .directory
            .borrow()
            .get(&device)
            .cloned()
            .ok_or(ServiceError::UnknownDevice)?;
        let ip = handle
            .snapshot()
            .link
            .map(|link| link.ip)
            .ok_or(ServiceError::NotConnected)?;
        let port = self.settings.effective(&device).screen.kiosk_port.get();
        let url = format!("http://{ip}:{port}/api/login");
        let body = json!({ "password": password, "ttl_days": TOKEN_DAYS }).to_string();
        let sent = self
            .http
            .post(&url)
            .timeout(LOGIN_TIMEOUT)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await;
        let response = match sent {
            Ok(response) => response,
            Err(error) => {
                tracing::info!(%device, %error, "Kiosk Satellite could not be reached");
                return Err(ServiceError::Upstream(format!(
                    "Kiosk Satellite did not answer on port {port}; check that remote administration is on"
                )));
            }
        };
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ServiceError::Invalid(
                "Kiosk Satellite refused that password".to_owned(),
            ));
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(ServiceError::Upstream(
                "Kiosk Satellite is refusing logins for now; wait and try again".to_owned(),
            ));
        }
        if !status.is_success() {
            return Err(ServiceError::Upstream(format!(
                "Kiosk Satellite answered {status}"
            )));
        }
        let bytes = response.bytes().await.map_err(|_| {
            ServiceError::Upstream("Kiosk Satellite's answer could not be read".to_owned())
        })?;
        let answer = serde_json::from_slice::<LoginAnswer>(&bytes)
            .ok()
            .filter(|answer| !answer.token.is_empty())
            .ok_or_else(|| {
                ServiceError::Upstream("Kiosk Satellite's answer had no token".to_owned())
            })?;
        let stored = self
            .db
            .write(StoreToken {
                device: device.clone(),
                token: answer.token.clone(),
            })
            .await
            .map_err(|error| ServiceError::Failed(format!("storing the kiosk token: {error}")))?;
        if !stored {
            return Err(ServiceError::UnknownDevice);
        }
        handle
            .set_kiosk_token(Some(answer.token))
            .map_err(|error| ServiceError::Unavailable(error.to_string()))?;
        tracing::info!(%device, "Kiosk Satellite token stored");
        Ok(())
    }
}

impl KioskLogin for KioskSatellite {
    fn login(&self, device: DeviceId, password: String) -> BoxFuture<'_, Result<(), ServiceError>> {
        Box::pin(self.login_now(device, password))
    }
}
