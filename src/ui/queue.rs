// SPDX-License-Identifier: GPL-3.0-or-later

use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::Span,
    widgets::{Block, Borders, Paragraph},
};

use super::{ACCENT, DIM};
use crate::app::App;

pub(super) fn render(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.queue_focused;
    let border_style = if focused {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(Color::Rgb(40, 40, 40))
    };
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(border_style);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let queue_title = if app.now_playing.shuffle {
        " Queue ⇄ "
    } else {
        " Queue "
    };
    f.render_widget(
        Paragraph::new(Span::styled(
            queue_title,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    let content_y = inner.y + 1;
    let content_h = inner.height.saturating_sub(1);
    let queue = &app.now_playing.queue;
    if queue.is_empty() {
        if content_h > 0 {
            f.render_widget(
                Paragraph::new("no queue")
                    .style(Style::default().fg(DIM))
                    .alignment(Alignment::Center),
                Rect::new(inner.x, content_y, inner.width, content_h),
            );
        }
        return;
    }

    let current = app.now_playing.queue_index;
    let cursor = app.queue_cursor;
    let item_h = 2usize;
    let visible = (content_h as usize).saturating_div(item_h).max(1);
    let offset = app.queue_scroll_offset(visible);

    let mut y = content_y;
    for (i, track) in queue.iter().enumerate().skip(offset) {
        if y + 1 >= content_y + content_h {
            break;
        }
        let is_cur = i == current;
        let is_cursor = focused && i == cursor && !app.help_active;
        let heart = if app.favorite_track_ids.contains(&track.id) { " ❤" } else { "" };
        let (title_line, line_style) = if is_cur {
            (
                format!("♪ {}{}", track.title, heart),
                Style::default()
                    .fg(Color::Rgb(180, 200, 255))
                    .add_modifier(Modifier::BOLD),
            )
        } else if is_cursor {
            (
                format!("▶ {}{}", track.title, heart),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )
        } else {
            (format!("{}{}", track.title, heart), Style::default().fg(Color::White))
        };

        f.render_widget(
            Paragraph::new(title_line).style(line_style),
            Rect::new(inner.x, y, inner.width, 1),
        );
        f.render_widget(
            Paragraph::new(format!("  {}", track.all_artist_names())).style(line_style),
            Rect::new(inner.x, y + 1, inner.width, 1),
        );
        y += item_h as u16;
    }
}
