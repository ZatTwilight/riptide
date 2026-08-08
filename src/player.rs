// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2025 Ryan Cohan

use anyhow::{bail, Result};
use serde_json::json;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::Duration;

const SOCKET_PATH: &str = "/tmp/riptide-mpv.sock";

#[derive(Debug)]
pub enum PlayerCmd {
    Play(String),
    Append(String),
    TogglePause,
    Stop,
    RemoveNext,
    SetMediaTitle(String),
    ChangeVolume(i8),
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    TrackStarted,
    TrackEnded,
    Position(f64),
    Duration(f64),
    Paused(bool),
    SampleRate(u32),
    Codec(String),
    Error(String),
    CurrVolume(u8),
}

pub struct PlayerWorker {
    cmd_rx: mpsc::UnboundedReceiver<PlayerCmd>,
    event_tx: mpsc::UnboundedSender<PlayerEvent>,
    initial_volume: u8,
}

impl PlayerWorker {
    pub fn new(
        cmd_rx: mpsc::UnboundedReceiver<PlayerCmd>,
        event_tx: mpsc::UnboundedSender<PlayerEvent>,
        initial_volume: u8,
    ) -> Self {
        Self { cmd_rx, event_tx, initial_volume: initial_volume.min(100) }
    }

    pub async fn run(mut self) {
        // Start mpv and keep the child handle so we can kill it on exit.
        let mut child = match self.start_mpv().await {
            Ok(c) => c,
            Err(e) => {
                let _ = self.event_tx.send(PlayerEvent::Error(e.to_string()));
                return;
            }
        };

        let stream = match self.connect_socket().await {
            Ok(s) => s,
            Err(e) => {
                let _ = self.event_tx.send(PlayerEvent::Error(e.to_string()));
                return;
            }
        };

        let (read_half, write_half) = stream.into_split();

        // Channel for IPC write commands
        let (ipc_tx, ipc_rx) = mpsc::unbounded_channel::<IpcRequest>();

        // Spawn write task
        let _write_task = tokio::spawn(write_loop(write_half, ipc_rx));

        // Spawn read task
        let event_tx_clone = self.event_tx.clone();
        let _read_task = tokio::spawn(read_loop(read_half, event_tx_clone));

        // mpv emits an initial value and then a property-change event whenever an
        // observed property changes, avoiding delayed polling and redundant IPC.
        for (id, property) in [
            (1, "time-pos"),
            (2, "duration"),
            (3, "pause"),
            (4, "audio-params"),
            (5, "audio-codec"),
            (6, "volume"),
        ] {
            let _ = ipc_tx.send(IpcRequest::Write(
                json!({"command": ["observe_property", id, property]}).to_string()
            ));
        }

        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                PlayerCmd::Play(url) => {
                    let _ = ipc_tx.send(IpcRequest::Write(
                        json!({"command": ["loadfile", url, "replace"]}).to_string()
                    ));
                    let _ = ipc_tx.send(IpcRequest::Write(
                        json!({"command": ["set_property", "pause", false]}).to_string()
                    ));
                }
                PlayerCmd::Append(url) => {
                    let _ = ipc_tx.send(IpcRequest::Write(
                        json!({"command": ["loadfile", url, "append"]}).to_string()
                    ));
                }
                PlayerCmd::TogglePause => {
                    let _ = ipc_tx.send(IpcRequest::Write(
                        json!({"command": ["cycle", "pause"]}).to_string()
                    ));
                }
                PlayerCmd::Stop => {
                    let _ = ipc_tx.send(IpcRequest::Write(
                        json!({"command": ["stop"]}).to_string()
                    ));
                }
                PlayerCmd::RemoveNext => {
                    let _ = ipc_tx.send(IpcRequest::Write(
                        json!({"command": ["playlist-remove", 1]}).to_string()
                    ));
                }
                PlayerCmd::SetMediaTitle(t) => {
                    let _ = ipc_tx.send(IpcRequest::Write(
                        json!({"command": ["set_property", "force-media-title", t]}).to_string()
                    ));
                }
                PlayerCmd::ChangeVolume(c) => {
                    let _ = ipc_tx.send(IpcRequest::Write(
                        json!({"command": ["add", "volume", c]}).to_string()
                    ));
                }
            }
        }

        // Clean up: kill mpv and remove the socket file.
        let _ = child.kill().await;
        let _ = std::fs::remove_file(SOCKET_PATH);
    }

    async fn start_mpv(&self) -> Result<tokio::process::Child> {
        if Path::new(SOCKET_PATH).exists() {
            let _ = std::fs::remove_file(SOCKET_PATH);
        }

        let socket_arg = format!("--input-ipc-server={SOCKET_PATH}");
        let volume_arg = format!("--volume={}", self.initial_volume);
        let child = Command::new("mpv")
            .args([
                "--no-video",
                "--idle=yes",
                socket_arg.as_str(),
                "--really-quiet",
                "--prefetch-playlist=yes",
                "--demuxer-max-bytes=32MiB",
                "--demuxer-max-back-bytes=8MiB",
                "--load-scripts=no",
                "--volume-max=100", // we don't want to go over 100, default is 130
                // stream cache: survives slow/flaky connections without stutter
                "--cache=yes",
                "--demuxer-max-bytes=150MiB",
                "--demuxer-readahead-secs=60",
                volume_arg.as_str(),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;

        Ok(child)
    }

    async fn connect_socket(&self) -> Result<UnixStream> {
        for _ in 0..50 {
            if Path::new(SOCKET_PATH).exists() {
                match UnixStream::connect(SOCKET_PATH).await {
                    Ok(s) => return Ok(s),
                    Err(_) => {}
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        bail!("mpv IPC socket did not appear – is mpv installed?")
    }
}

// ── IPC write loop ────────────────────────────────────────────────────────────

enum IpcRequest {
    Write(String),
}

async fn write_loop(
    mut writer: tokio::net::unix::OwnedWriteHalf,
    mut rx: mpsc::UnboundedReceiver<IpcRequest>,
) {
    while let Some(req) = rx.recv().await {
        match req {
            IpcRequest::Write(msg) => {
                let line = format!("{msg}\n");
                if writer.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        }
    }
}

// ── IPC read loop ─────────────────────────────────────────────────────────────

async fn read_loop(
    reader: tokio::net::unix::OwnedReadHalf,
    event_tx: mpsc::UnboundedSender<PlayerEvent>,
) {
    let mut buf = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        match buf.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }

        let Ok(data) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };

        if let Some(event) = player_event_from_mpv(&data) {
            let _ = event_tx.send(event);
        }
    }
}

fn player_event_from_mpv(data: &serde_json::Value) -> Option<PlayerEvent> {
    match data.get("event")?.as_str()? {
        "file-loaded" => Some(PlayerEvent::TrackStarted),
        "end-file" if data.get("reason").and_then(|v| v.as_str()) == Some("eof") => {
            Some(PlayerEvent::TrackEnded)
        }
        "property-change" => {
            let value = data.get("data")?;
            match data.get("name")?.as_str()? {
                "time-pos" => value.as_f64().map(PlayerEvent::Position),
                "duration" => value.as_f64().map(PlayerEvent::Duration),
                "pause" => value.as_bool().map(PlayerEvent::Paused),
                "audio-params" => value
                    .get("samplerate")
                    .and_then(|v| v.as_u64())
                    .map(|rate| PlayerEvent::SampleRate(rate as u32)),
                "audio-codec" => value
                    .as_str()
                    .map(|codec| PlayerEvent::Codec(codec.to_owned())),
                "volume" => value
                    .as_f64()
                    .map(|volume| PlayerEvent::CurrVolume(volume.clamp(0.0, 100.0) as u8)),
                _ => None,
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{player_event_from_mpv, PlayerEvent};
    use serde_json::json;

    #[test]
    fn parses_observed_pause_change() {
        assert_eq!(
            player_event_from_mpv(&json!({
                "event": "property-change",
                "id": 3,
                "name": "pause",
                "data": true
            })),
            Some(PlayerEvent::Paused(true))
        );
    }

    #[test]
    fn parses_observed_volume_change() {
        assert_eq!(
            player_event_from_mpv(&json!({
                "event": "property-change",
                "id": 6,
                "name": "volume",
                "data": 65.0
            })),
            Some(PlayerEvent::CurrVolume(65))
        );
    }

    #[test]
    fn ignores_end_file_when_track_was_replaced() {
        assert_eq!(
            player_event_from_mpv(&json!({"event": "end-file", "reason": "stop"})),
            None
        );
    }
}
