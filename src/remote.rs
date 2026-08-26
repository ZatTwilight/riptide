// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2025 Fezzik the Giant

//! Local HTTP API for external remotes (desktop dashboards, phones).
//!
//! Tidal's own API has no playback-control surface, so the app that plays the
//! audio is the only honest source of now-playing state and transport control.
//! This module serves a small HTTP+SSE API from inside riptide:
//!
//! - `GET /now-playing` — one-shot JSON snapshot of the player.
//! - `GET /events` — Server-Sent Events; meaningful changes push immediately,
//!   position advances are throttled and idle connections receive keepalives.
//! - `POST /play`, `/pause`, `/toggle`, `/next`, `/previous`, `/volume?v=0-100`
//!
//! All routes require `Authorization: Bearer <token>` (or `?token=<token>`,
//! for EventSource clients that cannot set headers) when a token is set.
//! It deliberately reuses the MPRIS control paths in the event loop, so the
//! two interfaces can never disagree about what the app does.

use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};

use crate::app::RepeatMode;

pub const DEFAULT_PORT: u16 = 16_385;
pub const DEFAULT_BIND: &str = "127.0.0.1";

// ── Config ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RemoteConfig {
    pub enabled: bool,
    pub port: u16,
    pub bind: String,
    /// Bearer token guarding every route. Generated on first enable; clearing
    /// it in the config regenerates one on the next start.
    pub token: Option<String>,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: DEFAULT_PORT,
            bind: DEFAULT_BIND.to_string(),
            token: None,
        }
    }
}

/// Generate a fresh bearer token (64 hex chars) for a newly enabled remote.
pub fn generate_token() -> String {
    use rand::Rng;
    let bytes: [u8; 32] = rand::thread_rng().r#gen();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ── State snapshot ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct NowPlaying {
    /// A track is loaded and the player has something to control.
    pub active: bool,
    pub paused: bool,
    pub track: Option<TrackInfo>,
    pub position_secs: f64,
    /// 0–100.
    pub volume: u8,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub queue_index: usize,
    pub queue_len: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

impl From<RepeatMode> for Repeat {
    fn from(mode: RepeatMode) -> Self {
        match mode {
            RepeatMode::Off => Self::Off,
            RepeatMode::All => Self::All,
            RepeatMode::One => Self::One,
        }
    }
}

#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct TrackInfo {
    pub id: u64,
    pub title: String,
    pub artists: Vec<String>,
    pub album: String,
    pub album_id: u64,
    /// Presentation-size cover URL on Tidal's CDN; the client fetches it directly.
    pub art_url: String,
    pub duration_secs: f64,
    /// Catalogue badge (MAX / ATMOS / HI-FI) — what the release offers.
    pub quality: Option<&'static str>,
    /// What Tidal actually delivered for this play.
    pub bit_depth: Option<i32>,
    pub sample_rate: Option<i32>,
    pub codec: Option<String>,
}

// ── Control commands ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub enum RemoteCmd {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
    /// 0–100.
    SetVolume(u8),
}

// ── Server ──────────────────────────────────────────────────────────────────

pub struct RemoteServer;

impl RemoteServer {
    pub async fn run(
        config: RemoteConfig,
        state: watch::Receiver<NowPlaying>,
        cmds: mpsc::UnboundedSender<RemoteCmd>,
    ) {
        if !config.enabled {
            return;
        }
        let addr = format!("{}:{}", config.bind, config.port);
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("remote API: failed to bind {addr}: {e}");
                return;
            }
        };
        tracing::info!("remote API listening on http://{addr}");
        loop {
            let accepted = tokio::select! {
                _ = cmds.closed() => return,
                accepted = listener.accept() => accepted,
            };
            match accepted {
                Ok((stream, _)) => {
                    let config = config.clone();
                    let state = state.clone();
                    let cmds = cmds.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(config, state, cmds, stream).await {
                            tracing::debug!("remote API connection ended: {e}");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("remote API accept failed: {e}");
                    return;
                }
            }
        }
    }
}

async fn handle_connection(
    config: RemoteConfig,
    mut state: watch::Receiver<NowPlaying>,
    cmds: mpsc::UnboundedSender<RemoteCmd>,
    mut stream: TcpStream,
) -> anyhow::Result<()> {
    // Requests here are tiny (no bodies); a short read is the common case and
    // the header block always ends within the first read for real clients.
    let mut buf = vec![0u8; 8192];
    let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf)).await??;
    let request = String::from_utf8_lossy(&buf[..n]);
    let mut lines = request.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };

    if !authorized(&config, &request, query) {
        respond(
            &mut stream,
            "401 Unauthorized",
            "application/json",
            Some(br#"{"error":"unauthorized"}"#),
        )
        .await?;
        return Ok(());
    }

    match (method, path) {
        ("GET", "/now-playing") => {
            let snapshot = state.borrow_and_update().clone();
            let body = serde_json::to_vec(&snapshot)?;
            respond(&mut stream, "200 OK", "application/json", Some(&body)).await?;
        }
        ("GET", "/events") => {
            run_sse(&mut stream, state).await?;
        }
        ("POST", "/play") => {
            let _ = cmds.send(RemoteCmd::Play);
            respond(&mut stream, "204 No Content", "", None).await?;
        }
        ("POST", "/pause") => {
            let _ = cmds.send(RemoteCmd::Pause);
            respond(&mut stream, "204 No Content", "", None).await?;
        }
        ("POST", "/toggle") => {
            let _ = cmds.send(RemoteCmd::Toggle);
            respond(&mut stream, "204 No Content", "", None).await?;
        }
        ("POST", "/next") => {
            let _ = cmds.send(RemoteCmd::Next);
            respond(&mut stream, "204 No Content", "", None).await?;
        }
        ("POST", "/previous") => {
            let _ = cmds.send(RemoteCmd::Previous);
            respond(&mut stream, "204 No Content", "", None).await?;
        }
        ("POST", "/volume") => match query_param(query, "v").and_then(|v| v.parse::<u8>().ok()) {
            Some(v) if v <= 100 => {
                let _ = cmds.send(RemoteCmd::SetVolume(v));
                respond(&mut stream, "204 No Content", "", None).await?;
            }
            _ => {
                respond(
                    &mut stream,
                    "400 Bad Request",
                    "application/json",
                    Some(br#"{"error":"expected ?v=0-100"}"#),
                )
                .await?;
            }
        },
        _ => {
            respond(
                &mut stream,
                "404 Not Found",
                "application/json",
                Some(br#"{"error":"not found"}"#),
            )
            .await?;
        }
    }
    Ok(())
}

/// Bearer token from the `Authorization` header, falling back to `?token=`.
/// The query fallback exists because browser `EventSource` cannot set headers.
fn authorized(config: &RemoteConfig, request: &str, query: &str) -> bool {
    let Some(expected) = config.token.as_deref() else {
        return true;
    };
    let header_ok = request
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .is_some_and(|(_, value)| value.trim() == format!("Bearer {expected}"));
    header_ok || query_param(query, "token").is_some_and(|t| t == expected)
}

fn query_param<'q>(query: &'q str, key: &str) -> Option<&'q str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

async fn run_sse(
    stream: &mut TcpStream,
    mut state: watch::Receiver<NowPlaying>,
) -> anyhow::Result<()> {
    let head = "HTTP/1.1 200 OK\r\n\
                Content-Type: text/event-stream\r\n\
                Cache-Control: no-cache\r\n\
                Connection: keep-alive\r\n\r\n";
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;

    let event = |snapshot: &NowPlaying| -> Vec<u8> {
        let mut json = serde_json::to_string(snapshot).unwrap_or_default();
        json.insert_str(0, "data: ");
        json.push_str("\n\n");
        json.into_bytes()
    };

    // The snapshot the client subscribes to must be the one current at connect.
    let mut last = state.borrow_and_update().clone();
    stream.write_all(&event(&last)).await?;
    stream.flush().await?;
    let mut last_position_emit = std::time::Instant::now();

    // Position updates arrive several times a second, far faster than a
    // dashboard needs: it can interpolate the elapsed offset itself between
    // events. Only a changed position is throttled; any other field change
    // (track, pause, volume…) pushes immediately.
    loop {
        match tokio::time::timeout(Duration::from_secs(15), state.changed()).await {
            Ok(Ok(())) => {
                let snapshot = state.borrow_and_update().clone();
                if !differs_only_in_position(&snapshot, &last)
                    || last_position_emit.elapsed() >= POSITION_EMIT_INTERVAL
                {
                    stream.write_all(&event(&snapshot)).await?;
                    stream.flush().await?;
                    last = snapshot;
                    last_position_emit = std::time::Instant::now();
                }
            }
            Ok(Err(_)) => {
                // State sender dropped: riptide is shutting down.
                return Ok(());
            }
            Err(_) => {
                stream.write_all(b": keepalive\n\n").await?;
                stream.flush().await?;
            }
        }
    }
}

/// How often a pure position advance is emitted on the SSE stream.
const POSITION_EMIT_INTERVAL: Duration = Duration::from_secs(5);

/// True when two snapshots differ only by the playhead moving forward — the
/// signal a client can interpolate for itself, and the one change we throttle.
fn differs_only_in_position(new: &NowPlaying, last: &NowPlaying) -> bool {
    let mut probe = new.clone();
    probe.position_secs = last.position_secs;
    probe == *last
}

async fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: Option<&[u8]>,
) -> anyhow::Result<()> {
    let mut writer = BufWriter::new(stream);
    let mut head = format!("HTTP/1.1 {status}\r\n");
    if !content_type.is_empty() {
        head.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.map(<[u8]>::len).unwrap_or(0)
    ));
    writer.write_all(head.as_bytes()).await?;
    if let Some(body) = body {
        writer.write_all(body).await?;
    }
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_param_extracts_values() {
        assert_eq!(query_param("v=50", "v"), Some("50"));
        assert_eq!(query_param("a=1&v=42", "v"), Some("42"));
        assert_eq!(query_param("a=1", "v"), None);
        assert_eq!(query_param("v=", "v"), Some(""));
    }

    #[test]
    fn authorization_header_is_matched_case_insensitively() {
        let config = RemoteConfig {
            enabled: true,
            token: Some("sekrit".to_string()),
            ..RemoteConfig::default()
        };
        let ok = "POST /play HTTP/1.1\r\nAuthorization: Bearer sekrit\r\n\r\n";
        assert!(authorized(&config, ok, ""));
        let wrong = "POST /play HTTP/1.1\r\nAuthorization: Bearer nope\r\n\r\n";
        assert!(!authorized(&config, wrong, ""));
        assert!(!authorized(&config, "POST /play HTTP/1.1\r\n\r\n", ""));
        assert!(!authorized(
            &config,
            "GET / HTTP/1.1\r\nxxxxxxxxxxxxxé: nope\r\n\r\n",
            ""
        ));
    }

    #[test]
    fn query_token_is_accepted_for_headerless_clients() {
        let config = RemoteConfig {
            enabled: true,
            token: Some("sekrit".to_string()),
            ..RemoteConfig::default()
        };
        assert!(authorized(
            &config,
            "GET /events HTTP/1.1\r\n\r\n",
            "token=sekrit"
        ));
        assert!(!authorized(
            &config,
            "GET /events HTTP/1.1\r\n\r\n",
            "token=nope"
        ));
    }

    #[test]
    fn no_token_configured_means_open_access() {
        let config = RemoteConfig::default();
        assert!(authorized(&config, "GET /now-playing HTTP/1.1\r\n\r\n", ""));
    }

    #[test]
    fn position_advance_is_distinguished_from_real_changes() {
        let base = NowPlaying {
            position_secs: 10.0,
            ..Default::default()
        };
        assert!(differs_only_in_position(
            &NowPlaying {
                position_secs: 12.5,
                ..base.clone()
            },
            &base
        ));
        assert!(!differs_only_in_position(
            &NowPlaying {
                position_secs: 12.5,
                paused: true,
                ..base.clone()
            },
            &base
        ));
        assert!(!differs_only_in_position(
            &NowPlaying {
                position_secs: 12.5,
                volume: 1,
                ..base.clone()
            },
            &base
        ));
    }

    #[test]
    fn repeat_maps_from_player_mode() {
        assert_eq!(Repeat::from(RepeatMode::One), Repeat::One);
        assert_eq!(Repeat::from(RepeatMode::All), Repeat::All);
        assert_eq!(Repeat::from(RepeatMode::Off), Repeat::Off);
    }

    #[tokio::test]
    async fn disabled_server_returns_without_waiting_for_commands() {
        let (_state_tx, state_rx) = watch::channel(NowPlaying::default());
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let config = RemoteConfig {
            port: 0,
            ..Default::default()
        };
        tokio::time::timeout(
            Duration::from_secs(1),
            RemoteServer::run(config, state_rx, cmd_tx),
        )
        .await
        .expect("disabled server must not start an accept loop");
    }

    #[tokio::test]
    async fn enabled_server_exits_when_command_receiver_closes() {
        let (_state_tx, state_rx) = watch::channel(NowPlaying::default());
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        drop(cmd_rx);
        let config = RemoteConfig {
            enabled: true,
            port: 0,
            ..Default::default()
        };
        tokio::time::timeout(
            Duration::from_secs(1),
            RemoteServer::run(config, state_rx, cmd_tx),
        )
        .await
        .expect("server must not keep the worker runtime alive after shutdown");
    }

    #[tokio::test]
    async fn server_serves_now_playing_and_requires_auth() -> anyhow::Result<()> {
        let (state_tx, state_rx) = watch::channel(NowPlaying::default());
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let config = RemoteConfig {
            enabled: true,
            port: 0, // unused; the test connects over an in-process listener below
            token: Some("sekrit".to_string()),
            ..RemoteConfig::default()
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let config = config.clone();
                let state_rx = state_rx.clone();
                let cmd_tx = cmd_tx.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(config, state_rx, cmd_tx, stream).await;
                });
            }
        });

        // Unauthorized without a token
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET /now-playing HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut buf = vec![0u8; 1024];
        let n = s.read(&mut buf).await.unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 401"));

        // Authorized request returns the snapshot and dispatches the command
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(
            b"POST /next HTTP/1.1\r\nAuthorization: Bearer sekrit\r\nContent-Length: 0\r\n\r\n",
        )
        .await
        .unwrap();
        let n = s.read(&mut buf).await.unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 204"));
        assert!(matches!(cmd_rx.recv().await, Some(RemoteCmd::Next)));

        state_tx
            .send(NowPlaying {
                volume: 42,
                ..Default::default()
            })
            .unwrap();
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET /now-playing HTTP/1.1\r\nAuthorization: Bearer sekrit\r\n\r\n")
            .await
            .unwrap();
        let n = s.read(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf[..n]);
        assert!(text.starts_with("HTTP/1.1 200"));
        assert!(text.contains(r#""volume":42"#));
        Ok(())
    }
}
