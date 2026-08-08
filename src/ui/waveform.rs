// SPDX-License-Identifier: GPL-3.0-or-later

use ratatui::{
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

use super::{ACCENT, DIM};
use crate::app::App;

/// Animated waveform: undulates while playing and becomes flat while paused.
pub(super) fn render(app: &App, width: u16) -> Paragraph<'static> {
    const WAVE: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

    let ratio = app.now_playing.progress_ratio();
    let played_width = ((width as f64 * ratio) as u16).min(width);
    let playing = app.now_playing.active && !app.now_playing.paused;
    let spans: Vec<Span<'static>> = (0..width)
        .map(|column| {
            let color = if column < played_width { ACCENT } else { DIM };
            let character = if playing {
                let phase = column as f64 * 0.8 + app.tick as f64 * 0.35;
                let normalized = (phase.sin() + 1.0) / 2.0;
                WAVE[(normalized * 7.99) as usize]
            } else {
                "▄"
            };
            Span::styled(character, Style::default().fg(color))
        })
        .collect();

    Paragraph::new(Line::from(spans))
}
