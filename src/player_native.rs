// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2025 Ryan Cohan

//! Native audio playback backed by Rodio and Symphonia.
//!
//! Stream setup runs in a cancellable task so a slow network request cannot
//! block stop, volume, pause, or replacement playback commands. The API client
//! supplies direct LOSSLESS FLAC or HIGH AAC/MP4 resources for this backend;
//! segmented FLAC and DASH manifests remain the responsibility of the mpv
//! backend, and transitions are therefore not gapless here.

use anyhow::{Context, Result};
use rodio::{Decoder, DeviceSinkBuilder, Player, Source};
use stream_download::source::DecodeError;
use stream_download::storage::temp::TempStorageProvider;
use stream_download::{Settings, StreamDownload};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Duration, MissedTickBehavior};

use crate::player::{PlayerCmd, PlayerEvent};

const POSITION_INTERVAL: Duration = Duration::from_millis(200);
const STREAM_OPEN_TIMEOUT: Duration = Duration::from_secs(30);
const STREAM_PREFETCH_BYTES: u64 = 512 * 1024;

type NativeDecoder = Decoder<StreamDownload<TempStorageProvider>>;

pub struct NativePlayerWorker {
    cmd_rx: mpsc::UnboundedReceiver<PlayerCmd>,
    event_tx: mpsc::UnboundedSender<PlayerEvent>,
    initial_volume: u8,
}

struct PreparedStream {
    decoder: NativeDecoder,
    metadata: StreamMetadata,
}

struct StreamLoadResult {
    generation: u64,
    result: Result<PreparedStream>,
}

struct PendingLoad {
    generation: u64,
    task: JoinHandle<()>,
}

impl NativePlayerWorker {
    pub fn new(
        cmd_rx: mpsc::UnboundedReceiver<PlayerCmd>,
        event_tx: mpsc::UnboundedSender<PlayerEvent>,
        initial_volume: u8,
    ) -> Self {
        Self {
            cmd_rx,
            event_tx,
            initial_volume: initial_volume.min(100),
        }
    }

    pub async fn run(mut self) {
        tracing::info!("native audio backend starting");
        let mut output = match DeviceSinkBuilder::open_default_sink() {
            Ok(output) => output,
            Err(error) => {
                tracing::error!(error = %error, "failed to open native audio output");
                self.send(PlayerEvent::Error(format!(
                    "open native audio output: {error}"
                )));
                return;
            }
        };
        output.log_on_drop(false);

        let player = Player::connect_new(output.mixer());
        let mut volume = self.initial_volume;
        player.set_volume(volume as f32 / 100.0);
        self.send(PlayerEvent::CurrVolume(volume));
        tracing::info!(initial_volume = volume, "native audio output ready");

        let (load_tx, mut load_rx) = mpsc::unbounded_channel();
        let mut load_generation = 0_u64;
        let mut pending_load: Option<PendingLoad> = None;
        // The next track's URL, held until the current resource reaches EOS.
        // Preparing it at EOS is not gapless, but avoids queueing an obsolete
        // decoder after ClearNext.
        let mut pending_url: Option<String> = None;
        let mut active = false;
        let mut paused = false;
        let mut ticker = tokio::time::interval(POSITION_INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                command = self.cmd_rx.recv() => {
                    let Some(command) = command else { break };
                    match command {
                        PlayerCmd::Play(url) => {
                            cancel_pending_load(&mut pending_load);
                            player.stop();
                            pending_url = None;
                            active = false;
                            paused = false;
                            load_generation = load_generation.wrapping_add(1);
                            pending_load =
                                Some(spawn_stream_load(load_generation, url, load_tx.clone()));
                        }
                        PlayerCmd::SetNext(url) => pending_url = Some(url),
                        PlayerCmd::ClearNext => pending_url = None,
                        PlayerCmd::TogglePause => {
                            let target = !paused;
                            set_paused(&player, &mut paused, target);
                            self.send(PlayerEvent::Paused(paused));
                        }
                        PlayerCmd::SetPaused(p) => {
                            if p != paused || player.is_paused() != p {
                                set_paused(&player, &mut paused, p);
                            }
                            self.send(PlayerEvent::Paused(paused));
                        }
                        PlayerCmd::Stop => {
                            tracing::debug!("native playback stopped");
                            cancel_pending_load(&mut pending_load);
                            load_generation = load_generation.wrapping_add(1);
                            player.stop();
                            pending_url = None;
                            active = false;
                            paused = false;
                        }
                        PlayerCmd::SetMediaTitle(_) => {}
                        PlayerCmd::ChangeVolume(change) => {
                            volume = adjusted_volume(volume, change);
                            player.set_volume(volume as f32 / 100.0);
                            self.send(PlayerEvent::CurrVolume(volume));
                        }
                        PlayerCmd::SetVolume(v) => {
                            volume = v.min(100);
                            player.set_volume(volume as f32 / 100.0);
                            self.send(PlayerEvent::CurrVolume(volume));
                        }
                        PlayerCmd::SeekAbsolute(secs) => {
                            if let Err(error) =
                                player.try_seek(Duration::from_secs_f64(secs.max(0.0)))
                            {
                                tracing::warn!(%error, secs, "native seek failed");
                            }
                        }
                    }
                }
                loaded = load_rx.recv(), if pending_load.is_some() => {
                    let Some(loaded) = loaded else { break };
                    if pending_load.as_ref().map(|load| load.generation)
                        != Some(loaded.generation)
                    {
                        continue;
                    }
                    pending_load = None;

                    match loaded.result {
                        Ok(stream) => {
                            let metadata = stream.metadata;
                            player.append(stream.decoder);
                            if paused {
                                player.pause();
                            } else {
                                player.play();
                            }
                            active = true;
                            self.send(PlayerEvent::TrackStarted);
                            emit_metadata(&self.event_tx, metadata);
                        }
                        Err(error) => {
                            active = false;
                            let message = format!("{error:#}");
                            tracing::error!(error = %message, "native stream failed");
                            if error.is::<rodio::decoder::DecoderError>() {
                                self.send(PlayerEvent::TrackFailed(message));
                                if let Some(url) = pending_url.take() {
                                    load_generation = load_generation.wrapping_add(1);
                                    pending_load = Some(spawn_stream_load(
                                        load_generation,
                                        url,
                                        load_tx.clone(),
                                    ));
                                }
                            } else {
                                self.send(PlayerEvent::Error(message));
                            }
                        }
                    }
                }
                _ = ticker.tick() => {
                    if active && player.empty() {
                        tracing::info!("native track ended");
                        self.send(PlayerEvent::TrackEnded);
                        active = false;

                        if let Some(url) = pending_url.take() {
                            load_generation = load_generation.wrapping_add(1);
                            pending_load =
                                Some(spawn_stream_load(load_generation, url, load_tx.clone()));
                        }
                    } else if active && !paused {
                        self.send(PlayerEvent::Position(player.get_pos().as_secs_f64()));
                    }
                }
            }
        }

        cancel_pending_load(&mut pending_load);
        player.stop();
        tracing::info!("native audio backend stopped");
    }

    fn send(&self, event: PlayerEvent) {
        let _ = self.event_tx.send(event);
    }
}

fn set_paused(player: &Player, paused: &mut bool, target: bool) {
    *paused = target;
    if target {
        player.pause();
    } else {
        player.play();
    }
}

fn spawn_stream_load(
    generation: u64,
    url: String,
    result_tx: mpsc::UnboundedSender<StreamLoadResult>,
) -> PendingLoad {
    let task = tokio::spawn(async move {
        let result = match tokio::time::timeout(STREAM_OPEN_TIMEOUT, prepare_stream(&url)).await {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "timed out opening native stream after {} seconds",
                STREAM_OPEN_TIMEOUT.as_secs()
            )),
        };
        let _ = result_tx.send(StreamLoadResult { generation, result });
    });
    PendingLoad { generation, task }
}

fn cancel_pending_load(pending: &mut Option<PendingLoad>) {
    if let Some(load) = pending.take() {
        load.task.abort();
    }
}

struct StreamMetadata {
    duration: Option<Duration>,
    sample_rate: u32,
    header_quality: Option<crate::api::models::DeliveredQuality>,
    codec: &'static str,
}

fn flac_header_quality(header: &[u8]) -> Option<crate::api::models::DeliveredQuality> {
    if header.len() < 42
        || &header[..4] != b"fLaC"
        || header[4] & 0x7f != 0
        || header[5..8] != [0, 0, 34]
    {
        return None;
    }
    let packed = u64::from_be_bytes(header[18..26].try_into().ok()?);
    Some(crate::api::models::DeliveredQuality {
        sample_rate: Some((packed >> 44) as i32),
        bit_depth: Some(((packed >> 36) & 0x1f) as i32 + 1),
    })
}

async fn prepare_stream(url: &str) -> Result<PreparedStream> {
    tracing::info!("opening native stream");
    let parsed = url.parse().context("parse native stream URL")?;
    let reader = match StreamDownload::new_http(
        parsed,
        TempStorageProvider::with_prefix("riptide-audio-"),
        Settings::default().prefetch_bytes(STREAM_PREFETCH_BYTES),
    )
    .await
    {
        Ok(reader) => reader,
        Err(error) => return Err(anyhow::anyhow!(error.decode_error().await)),
    };

    // Symphonia's header parse reads through stream-download's `Read` impl,
    // which parks the calling OS thread on a condvar until the download task
    // delivers bytes. That must not occupy an async worker — run it on the
    // blocking pool. (Playback reads happen on rodio's own audio thread, so
    // the decoder is safe to hand over afterwards.)
    let decoder = tokio::task::spawn_blocking(move || -> Result<PreparedStream> {
        use std::io::{Read, Seek, SeekFrom};
        let mut reader = reader;
        let mut header = [0_u8; 42];
        reader
            .read_exact(&mut header)
            .context("read audio header")?;
        reader
            .seek(SeekFrom::Start(0))
            .context("rewind audio header")?;
        let header_quality = flac_header_quality(&header);
        let codec = if header_quality.is_some() {
            "Symphonia · FLAC"
        } else {
            "Symphonia"
        };
        Decoder::builder()
            .with_data(reader)
            .with_gapless(true)
            .build()
            .map(|decoder| {
                let metadata = StreamMetadata {
                    duration: decoder.total_duration(),
                    sample_rate: decoder.sample_rate().get(),
                    header_quality,
                    codec,
                };
                tracing::info!(
                    sample_rate = metadata.sample_rate,
                    bit_depth = header_quality.and_then(|q| q.bit_depth),
                    header_sample_rate = header_quality.and_then(|q| q.sample_rate),
                    codec,
                    duration_secs = metadata.duration.map(|duration| duration.as_secs_f64()),
                    "native stream decoded successfully"
                );
                PreparedStream { decoder, metadata }
            })
            .map_err(anyhow::Error::from)
    })
    .await
    .context("native decode task panicked")?
    .context("decode Tidal stream with Symphonia")?;
    Ok(decoder)
}

fn adjusted_volume(current: u8, change: i8) -> u8 {
    (i16::from(current) + i16::from(change)).clamp(0, 100) as u8
}

fn emit_metadata(event_tx: &mpsc::UnboundedSender<PlayerEvent>, metadata: StreamMetadata) {
    let _ = event_tx.send(PlayerEvent::SampleRate(metadata.sample_rate));
    let _ = event_tx.send(PlayerEvent::Codec(metadata.codec.to_owned()));
    if let Some(quality) = metadata.header_quality {
        let _ = event_tx.send(PlayerEvent::HeaderQuality(quality));
    }
    if let Some(duration) = metadata.duration {
        let _ = event_tx.send(PlayerEvent::Duration(duration.as_secs_f64()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_flac_source_resolution() {
        for (rate, depth) in [(44100_u64, 16_u64), (192000, 24)] {
            let mut header = [0_u8; 42];
            header[..4].copy_from_slice(b"fLaC");
            header[7] = 34;
            let packed = (rate << 44) | ((depth - 1) << 36);
            header[18..26].copy_from_slice(&packed.to_be_bytes());
            let quality = flac_header_quality(&header).unwrap();
            assert_eq!(quality.sample_rate, Some(rate as i32));
            assert_eq!(quality.bit_depth, Some(depth as i32));
        }
        assert!(flac_header_quality(b"not FLAC").is_none());
    }

    /// Network probe mirroring the production load path (spawn + timeout +
    /// blocking-pool decode). Run with:
    /// cargo test --bins native_stream_probe -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn native_stream_probe() {
        let config = crate::api::auth::load_config().expect("load config");
        let client = crate::api::client::ApiClient::new(config);
        let (url, delivered) = client
            .get_stream_url(431291038)
            .await
            .expect("resolve failed");
        eprintln!("probe: url resolved");

        let prepared = tokio::time::timeout(
            STREAM_OPEN_TIMEOUT,
            tokio::task::spawn(async move { prepare_stream(&url).await }),
        )
        .await
        .expect("probe timed out")
        .expect("join failed")
        .expect("prepare failed");
        eprintln!(
            "probe: SUCCESS codec={} decoded_rate={} header={:?} tidal={:?}",
            prepared.metadata.codec,
            prepared.metadata.sample_rate,
            prepared.metadata.header_quality,
            delivered
        );
    }
}
