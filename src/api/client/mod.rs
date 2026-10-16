// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2025 Fezzik the Giant

//! Tidal API client.
//!
//! [`ApiClient`] holds the HTTP core — auth, the JSON:API request helpers and
//! the shared constants. Its methods are implemented across the domain modules
//! below, each of which owns both its requests and the parsers for their
//! responses.

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use tokio::sync::{Mutex, RwLock};

use super::models::*;

mod parse;

mod albums;
mod artists;
mod mixes;
mod playlists;
mod radio;
mod search;
mod streaming;
mod tracks;

pub use search::{SearchArtistPage, SearchPlaylistPage, SearchTrackPage};

const BASE: &str = "https://api.tidal.com/v1";
const OPENAPI_BASE: &str = "https://openapi.tidal.com/v2";

// Private types for the openapi.tidal.com/v2 JSON:API collection endpoints.
#[derive(serde::Deserialize)]
pub(super) struct OpenApiRelPage {
    data: Vec<OpenApiRelItem>,
    #[serde(default)]
    included: Vec<OpenApiIncluded>,
    links: Option<OpenApiLinks>,
}

#[derive(serde::Deserialize)]
pub(super) struct OpenApiRelItem {
    id: String,
    meta: Option<OpenApiItemMeta>,
}

#[derive(serde::Deserialize)]
pub(super) struct OpenApiItemMeta {
    #[serde(rename = "addedAt")]
    added_at: Option<String>,
}

#[derive(serde::Deserialize)]
pub(super) struct OpenApiIncluded {
    id: String,
    attributes: Option<OpenApiPlaylistAttrs>,
}

#[derive(serde::Deserialize)]
pub(super) struct OpenApiPlaylistAttrs {
    name: String,
    #[serde(rename = "numberOfItems")]
    number_of_items: Option<u32>,
}

#[derive(serde::Deserialize)]
pub(super) struct OpenApiLinks {
    meta: Option<OpenApiLinksMeta>,
}

#[derive(serde::Deserialize)]
pub(super) struct OpenApiLinksMeta {
    #[serde(rename = "nextCursor")]
    next_cursor: Option<String>,
}
const USER_AGENT: &str = "Mozilla/5.0 (Linux; Android 12; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/91.0.4472.114 Safari/537.36";

pub struct ApiClient {
    http: reqwest::Client,
    token: RwLock<String>,
    config: RwLock<Config>,
    refresh_lock: Mutex<()>,
}

impl ApiClient {
    pub fn new(config: Config) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .expect("failed to build HTTP client");
        let token = config.access_token.clone().unwrap_or_default();
        Self {
            http,
            token: RwLock::new(token),
            config: RwLock::new(config),
            refresh_lock: Mutex::new(()),
        }
    }

    async fn country_code(&self) -> String {
        self.config.read().await.country_code.clone()
    }

    async fn session_id(&self) -> Option<String> {
        self.config.read().await.session_id.clone()
    }

    async fn player_backend(&self) -> PlayerBackend {
        self.config.read().await.player_backend
    }

    pub(super) async fn get_authenticated(
        &self,
        url: &str,
        accept: Option<&str>,
    ) -> Result<reqwest::Response> {
        let token = self.token.read().await.clone();
        let send = |token: &str| {
            let mut request = self.http.get(url).bearer_auth(token);
            if let Some(accept) = accept {
                request = request.header("Accept", accept);
            }
            request.send()
        };

        let response = send(&token).await.context("authenticated GET failed")?;
        if response.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(response);
        }

        let body = response.text().await.unwrap_or_default();
        if self.try_refresh_token(&token).await {
            let refreshed = self.token.read().await.clone();
            return send(&refreshed)
                .await
                .context("authenticated GET retry failed");
        }

        anyhow::bail!(
            "HTTP 401 Unauthorized: {}",
            body.chars().take(400).collect::<String>()
        );
    }

    async fn try_refresh_token(&self, failed_token: &str) -> bool {
        if *self.token.read().await != failed_token {
            return true;
        }
        let _guard = self.refresh_lock.lock().await;
        if *self.token.read().await != failed_token {
            return true;
        }
        let (refresh_token, client_id, client_secret) = {
            let cfg = self.config.read().await;
            let rt = match cfg.refresh_token.clone() {
                Some(rt) => rt,
                None => return false,
            };
            let cid = cfg
                .client_id
                .clone()
                .unwrap_or_else(|| crate::api::auth::DEFAULT_CLIENT_ID.to_string());
            let cs = cfg
                .client_secret
                .clone()
                .unwrap_or_else(|| crate::api::auth::DEFAULT_CLIENT_SECRET.to_string());
            (rt, cid, cs)
        };
        tracing::info!("access token expired, refreshing");
        let resp = match self
            .http
            .post("https://auth.tidal.com/v1/oauth2/token")
            .basic_auth(&client_id, Some(&client_secret))
            .form(&[
                ("client_id", client_id.as_str()),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token.as_str()),
            ])
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("token refresh request failed: {e}");
                return false;
            }
        };
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!(
                "token refresh failed {status}: {}",
                body.chars().take(200).collect::<String>()
            );
            return false;
        }
        let token: TokenResponse = match resp.json().await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("token refresh parse failed: {e}");
                return false;
            }
        };
        let new_access = token.access_token.clone();
        {
            let mut cfg = self.config.write().await;
            cfg.access_token = Some(new_access.clone());
            if let Some(rt) = token.refresh_token.clone() {
                cfg.refresh_token = Some(rt);
            }
            let expires_at =
                chrono::Utc::now() + chrono::Duration::seconds(token.expires_in as i64);
            cfg.expires_at = Some(expires_at.to_rfc3339());
            if let Some(user) = &token.user {
                cfg.user_id = Some(user.user_id);
                if !user.country_code.is_empty() {
                    cfg.country_code = user.country_code.clone();
                }
            }
            if cfg.country_code.is_empty() {
                cfg.country_code = "US".to_string();
            }
            let to_save = cfg.clone();
            drop(cfg);
            if let Err(e) = crate::api::auth::save_config(&to_save) {
                tracing::warn!("failed to persist refreshed token: {e}");
            }
        }
        *self.token.write().await = new_access.clone();
        if let Ok(sess_resp) = self
            .http
            .get("https://api.tidal.com/v1/sessions")
            .bearer_auth(&new_access)
            .send()
            .await
        {
            if sess_resp.status().is_success() {
                if let Ok(info) = sess_resp.json::<SessionInfo>().await {
                    let mut cfg = self.config.write().await;
                    cfg.session_id = Some(info.session_id);
                    cfg.user_id = Some(info.user_id);
                    if !info.country_code.is_empty() {
                        cfg.country_code = info.country_code;
                    }
                    let to_save = cfg.clone();
                    drop(cfg);
                    let _ = crate::api::auth::save_config(&to_save);
                }
            }
        }
        tracing::info!("token refreshed successfully");
        true
    }

    async fn get_openapi<T: DeserializeOwned>(
        &self,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<T> {
        let cc = self.country_code().await;
        let url = format!("{OPENAPI_BASE}{path}");
        let mut all_params = vec![("countryCode", cc)];
        all_params.extend_from_slice(params);
        let token = self.token.read().await.clone();
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .query(&all_params)
            .send()
            .await
            .context("openapi GET failed")?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            let body = resp.text().await.unwrap_or_default();
            if body.contains("Expired token") || body.contains("UNAUTHORIZED") {
                if self.try_refresh_token(&token).await {
                    let new_token = self.token.read().await.clone();
                    let retry = self
                        .http
                        .get(&url)
                        .bearer_auth(&new_token)
                        .query(&all_params)
                        .send()
                        .await
                        .context("openapi GET retry failed")?;
                    let bytes = retry.error_for_status()?.bytes().await?;
                    return serde_json::from_slice::<T>(&bytes).map_err(|e| {
                        let snippet: String =
                            String::from_utf8_lossy(&bytes).chars().take(400).collect();
                        anyhow::anyhow!("{e} — body: {snippet}")
                    });
                }
            }
            anyhow::bail!(
                "HTTP 401 Unauthorized: {}",
                body.chars().take(400).collect::<String>()
            );
        }
        let bytes = resp.error_for_status()?.bytes().await?;
        serde_json::from_slice::<T>(&bytes).map_err(|e| {
            let snippet: String = String::from_utf8_lossy(&bytes).chars().take(400).collect();
            anyhow::anyhow!("{e} — body: {snippet}")
        })
    }

    async fn post_openapi_json(&self, path: &str, body: &serde_json::Value) -> Result<()> {
        let cc = self.country_code().await;
        let token = self.token.read().await.clone();
        let url = format!("{OPENAPI_BASE}{path}");
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .query(&[("countryCode", &cc)])
            .json(body)
            .send()
            .await
            .context("openapi POST failed")?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            let b = resp.text().await.unwrap_or_default();
            if (b.contains("Expired token") || b.contains("UNAUTHORIZED"))
                && self.try_refresh_token(&token).await
            {
                let nt = self.token.read().await.clone();
                self.http
                    .post(&url)
                    .bearer_auth(&nt)
                    .query(&[("countryCode", &self.country_code().await)])
                    .json(body)
                    .send()
                    .await
                    .context("openapi POST retry failed")?
                    .error_for_status()?;
                return Ok(());
            }
            anyhow::bail!(
                "HTTP 401 Unauthorized: {}",
                b.chars().take(400).collect::<String>()
            );
        }
        resp.error_for_status()?;
        Ok(())
    }

    async fn delete_openapi_json(&self, path: &str, body: &serde_json::Value) -> Result<()> {
        let cc = self.country_code().await;
        let token = self.token.read().await.clone();
        let url = format!("{OPENAPI_BASE}{path}");
        let resp = self
            .http
            .delete(&url)
            .bearer_auth(&token)
            .query(&[("countryCode", &cc)])
            .json(body)
            .send()
            .await
            .context("openapi DELETE failed")?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            let b = resp.text().await.unwrap_or_default();
            if (b.contains("Expired token") || b.contains("UNAUTHORIZED"))
                && self.try_refresh_token(&token).await
            {
                let nt = self.token.read().await.clone();
                self.http
                    .delete(&url)
                    .bearer_auth(&nt)
                    .query(&[("countryCode", &self.country_code().await)])
                    .json(body)
                    .send()
                    .await
                    .context("openapi DELETE retry failed")?
                    .error_for_status()?;
                return Ok(());
            }
            anyhow::bail!(
                "HTTP 401 Unauthorized: {}",
                b.chars().take(400).collect::<String>()
            );
        }
        resp.error_for_status()?;
        Ok(())
    }

    // ── Artists ───────────────────────────────────────────────────────────────

    // ── Playlists ─────────────────────────────────────────────────────────────

    // ── Favorites ─────────────────────────────────────────────────────────────

    // ── Search ────────────────────────────────────────────────────────────────

    // ── Albums ────────────────────────────────────────────────────────────────

    // ── Radio ─────────────────────────────────────────────────────────────────

    // ── Lyrics ───────────────────────────────────────────────────────────────

    // ── Playback ──────────────────────────────────────────────────────────────

    /// Fetch raw bytes from a public URL (e.g. Tidal's cover art CDN).
    pub async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let resp = self.http.get(url).send().await?;
        let status = resp.status();

        let bytes = resp
            .error_for_status()
            .map_err(|e| {
                tracing::warn!("Failed to fetch bytes ({}): {}", status.as_u16(), e);
                e
            })?
            .bytes()
            .await?;

        tracing::debug!("Fetched {} bytes", bytes.len());
        Ok(bytes.to_vec())
    }
}

/// Resolve a JSON:API `links.next` value against the v2 base.
///
/// These endpoints return `next` as a path (`/userCollectionAlbums/...`), not an
/// absolute URL, so following it verbatim produces an unusable request.
pub(super) fn absolute_url(url: &str) -> String {
    if url.starts_with("http") {
        url.to_string()
    } else {
        format!("{OPENAPI_BASE}{url}")
    }
}

// ── Multi-segment FLAC playlist ────────────────────────────────────────────────

// ── DASH → HLS conversion ─────────────────────────────────────────────────────

#[cfg(test)]
mod url_tests {
    use super::absolute_url;

    #[test]
    fn a_relative_next_link_is_resolved_against_the_v2_base() {
        // links.next comes back as a path; following it verbatim is not a URL.
        assert_eq!(
            absolute_url("/userCollectionAlbums/me/relationships/items?page%5Bcursor%5D=abc"),
            "https://openapi.tidal.com/v2/userCollectionAlbums/me/relationships/items?page%5Bcursor%5D=abc"
        );
    }

    #[test]
    fn an_absolute_next_link_is_left_alone() {
        let url = "https://openapi.tidal.com/v2/userCollectionAlbums/me?page=2";
        assert_eq!(absolute_url(url), url);
    }
}
