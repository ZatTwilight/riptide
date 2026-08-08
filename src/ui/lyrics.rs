// SPDX-License-Identifier: GPL-3.0-or-later

use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    widgets::Paragraph,
};

use super::{DIM, spinner_char};
use crate::app::App;

pub(super) fn render(f: &mut Frame, app: &App, area: Rect) {
    let np = &app.now_playing;

    if np.lyrics_loading {
        f.render_widget(
            Paragraph::new(spinner_char(app.tick).to_string())
                .style(Style::default().fg(DIM))
                .alignment(Alignment::Center),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
        return;
    }

    let lines: &[(f64, String)];
    let plain_buf: Vec<(f64, String)>;
    if !np.lyrics_synced.is_empty() {
        lines = &np.lyrics_synced;
    } else if !np.lyrics_plain.is_empty() {
        let n = np.lyrics_plain.len() as f64;
        let duration = if np.duration > 0.0 { np.duration } else { n };
        plain_buf = np
            .lyrics_plain
            .iter()
            .enumerate()
            .map(|(i, text)| (i as f64 / n * duration, text.clone()))
            .collect();
        lines = &plain_buf;
    } else {
        return;
    }

    let current = lines
        .partition_point(|(timestamp, _)| *timestamp <= np.position)
        .saturating_sub(1);
    let visible = [
        current.checked_sub(1),
        Some(current),
        (current + 1 < lines.len()).then_some(current + 1),
    ];

    for (row, index) in visible.into_iter().enumerate() {
        let Some(index) = index else { continue };
        let style = if row == 1 {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(DIM)
        };
        let y = area.y + row as u16;
        if y < area.bottom() {
            f.render_widget(
                Paragraph::new(lines[index].1.as_str())
                    .style(style)
                    .alignment(Alignment::Center),
                Rect::new(area.x, y, area.width, 1),
            );
        }
    }
}
