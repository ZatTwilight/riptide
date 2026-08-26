// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2025 Fezzik the Giant

//! Terminal image rendering.
//!
//! Protocols are cached by content, size, and resize behavior; see
//! [`PROTOCOL_CACHE`] for why that matters for render latency.

use ratatui::Frame;
use ratatui::layout::{Rect, Size};
use ratatui_image::{
    FilterType, Image, Resize,
    picker::{Picker, ProtocolType},
    protocol::{Protocol, kitty::Kitty},
};

use crate::app::{CachedImage, image_content_hash};

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum ImageResize {
    Fit,
    Scale,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct ProtocolCacheKey {
    content_hash: u64,
    width: u16,
    height: u16,
    resize: ImageResize,
}

struct CachedProtocol {
    protocol: Protocol,
    kitty_id: Option<u32>,
}

/// Which overlays are painted over the frame, one bit each. A single bool
/// cannot distinguish "help closed" from "help closed while a toast is still
/// up", and only the first exposes the cells underneath.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Overlays(u8);

impl Overlays {
    pub(super) const COMMAND: Self = Self(1 << 0);
    pub(super) const SORT: Self = Self(1 << 1);
    pub(super) const ARTIST_PICKER: Self = Self(1 << 2);
    pub(super) const HELP: Self = Self(1 << 3);
    pub(super) const STATUS: Self = Self(1 << 4);

    pub(super) const fn none() -> Self {
        Self(0)
    }

    pub(super) const fn with(self, other: Self, active: bool) -> Self {
        if active { Self(self.0 | other.0) } else { self }
    }

    const fn any_closed_since(self, previous: Self) -> bool {
        previous.0 & !self.0 != 0
    }
}

#[derive(Clone, Copy, Default)]
struct ImageFrameState {
    fullscreen_art: bool,
    overlays: Overlays,
}

thread_local! {
    /// Cached terminal-image protocols, keyed by image content, target size,
    /// and resize behavior.
    ///
    /// Building a protocol decodes the source image and re-encodes it for the
    /// terminal's graphics protocol — ~760 µs for a 320x320 JPEG under Kitty.
    /// A rebuild produces a new payload, so ratatui's buffer diff cannot skip
    /// the whole ~135 KiB escape sequence. With two images on screen — the
    /// now-playing art plus a detail view's art — that came to ~16 MiB/s at 60
    /// fps, which is what made the cursor lag in detail views.
    ///
    /// Cached protocols render byte-identical cells on repeat frames, so the
    /// diff emits nothing once the image has been sent. Kitty protocols also
    /// use a fixed ID pool so replacement bounds terminal-side image memory.
    static PROTOCOL_CACHE: std::cell::RefCell<
        std::collections::HashMap<ProtocolCacheKey, Option<CachedProtocol>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());

    /// Tracks renderer-visible transitions that invalidate the scaled Kitty
    /// placement. Keeping this beside the cache means callers do not need to
    /// know when a terminal image protocol must be rebuilt.
    static IMAGE_FRAME_STATE: std::cell::Cell<ImageFrameState> =
        const { std::cell::Cell::new(ImageFrameState {
            fullscreen_art: false,
            overlays: Overlays::none(),
        }) };
}

// Initialize picker once at startup to avoid blocking on every frame
pub(super) fn get_picker() -> &'static Picker {
    static PICKER: std::sync::OnceLock<Picker> = std::sync::OnceLock::new();
    PICKER.get_or_init(|| {
        let term = std::env::var("TERM").unwrap_or_else(|_| "unknown".to_string());
        let colorterm = std::env::var("COLORTERM").unwrap_or_else(|_| "not set".to_string());
        let picker = match Picker::from_query_stdio() {
            Ok(picker) => picker,
            Err(error) => {
                tracing::warn!(%error, "failed to detect terminal image protocol; using halfblocks");
                Picker::halfblocks()
            }
        };
        tracing::info!(
            "Terminal: TERM={}, COLORTERM={} → Image protocol: {:?}",
            term,
            colorterm,
            picker.protocol_type()
        );
        picker
    })
}

/// Only a couple of (image, size) pairs are ever live at once, so a small cap
/// with a wholesale clear is enough to bound growth as the user browses.
pub(super) const PROTOCOL_CACHE_CAP: usize = 8;
const KITTY_IMAGE_ID_START: u32 = 0x5250_0000;

fn delete_terminal_image_data() {
    if get_picker().protocol_type() != ProtocolType::Kitty {
        return;
    }

    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    let result = (KITTY_IMAGE_ID_START..KITTY_IMAGE_ID_START + PROTOCOL_CACHE_CAP as u32)
        .try_for_each(|id| write!(stdout, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\"))
        .and_then(|_| stdout.flush());
    if let Err(error) = result {
        tracing::warn!(%error, "failed to delete terminal artwork data");
    }
}

pub(super) fn clear_protocol_cache() {
    let had_kitty_protocols = PROTOCOL_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let had_kitty_protocols = cache
            .values()
            .flatten()
            .any(|cached| cached.kitty_id.is_some());
        cache.clear();
        had_kitty_protocols
    });
    if had_kitty_protocols {
        delete_terminal_image_data();
    }
}

/// Dropping a cached `Protocol` resets its transmit latch, so the next frame
/// re-sends the whole image. Fixed Kitty IDs let that transmission replace the
/// superseded terminal-side image instead of growing Ghostty's heap forever.
fn release_scaled_protocols() {
    PROTOCOL_CACHE.with(|cache| {
        cache
            .borrow_mut()
            .retain(|key, _| key.resize != ImageResize::Scale);
    });
}

pub(super) fn prepare_image_frame(fullscreen_art: bool, overlays: Overlays) {
    let next = ImageFrameState {
        fullscreen_art,
        overlays,
    };
    let release_scaled = IMAGE_FRAME_STATE.with(|state| {
        let previous = state.replace(next);
        should_release_scaled_protocol(previous, next)
    });

    if release_scaled {
        release_scaled_protocols();
    }
}

fn should_release_scaled_protocol(previous: ImageFrameState, next: ImageFrameState) -> bool {
    // Closing an overlay exposes cells that the overlay overwrote. Rebuilding
    // the virtual placement makes the terminal paint those cells again.
    (previous.fullscreen_art && !next.fullscreen_art)
        || (next.fullscreen_art && next.overlays.any_closed_since(previous.overlays))
}

/// The terminal's cell size in pixels, queried once at startup.
///
/// Needed to frame an image snugly: `Resize::Fit` preserves the source's aspect
/// ratio, so a box laid out on a guessed cell ratio leaves the border floating
/// clear of the picture on any font that is not exactly twice as tall as wide.
pub(super) fn cell_size() -> (u16, u16) {
    let size = get_picker().font_size();
    (size.width.max(1), size.height.max(1))
}

/// How many whole cells `bytes` covers at its natural size.
///
/// `Resize::Fit` only ever scales an image *down* — an image smaller than its
/// area renders at native size and leaves the rest blank. So a frame drawn
/// around one has to be capped at this, or the border floats clear of the
/// picture. Reads the header only, not the pixels.
pub(super) fn image_cells(bytes: &[u8]) -> Option<(u16, u16)> {
    let (w, h) = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    let (cell_w, cell_h) = cell_size();
    Some((
        (w / cell_w as u32).max(1) as u16,
        (h / cell_h as u32).max(1) as u16,
    ))
}

pub(super) fn render_image(f: &mut Frame, bytes: &[u8], area: Rect) -> bool {
    render_image_with_resize(f, bytes, area, ImageResize::Fit, image_content_hash(bytes))
}

pub(super) fn render_scaled_image(f: &mut Frame, image: &CachedImage, area: Rect) -> bool {
    render_image_with_resize(
        f,
        image.bytes(),
        area,
        ImageResize::Scale,
        image.content_hash(),
    )
}

fn render_image_with_resize(
    f: &mut Frame,
    bytes: &[u8],
    area: Rect,
    resize: ImageResize,
    content_hash: u64,
) -> bool {
    if area.width == 0 || area.height == 0 {
        return false;
    }

    let key = ProtocolCacheKey {
        content_hash,
        width: area.width,
        height: area.height,
        resize,
    };

    PROTOCOL_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();

        if !cache.contains_key(&key) {
            // A scaled Kitty protocol can retain a multi-megabyte RGBA
            // transmission. Fullscreen redraws and track changes supersede one
            // another, so keeping more than the current one can
            // multiply RSS without making a future frame faster.
            prune_superseded_scaled_protocols(&mut cache, key);
            if cache.len() >= PROTOCOL_CACHE_CAP {
                let had_kitty_protocols = cache
                    .values()
                    .flatten()
                    .any(|cached| cached.kitty_id.is_some());
                cache.clear();
                if had_kitty_protocols {
                    delete_terminal_image_data();
                }
            }

            let img = match image::load_from_memory(bytes) {
                Ok(img) => img,
                Err(error) => {
                    tracing::warn!(%error, bytes = bytes.len(), "failed to decode artwork");
                    // Cache the failure so corrupt bytes do not get decoded and
                    // logged again on every draw tick.
                    cache.insert(key, None);
                    return false;
                }
            };
            let protocol_resize = match resize {
                ImageResize::Fit => Resize::Fit(None),
                ImageResize::Scale => Resize::Scale(Some(FilterType::CatmullRom)),
            };
            let picker = get_picker();
            let (protocol, kitty_id) = if picker.protocol_type() == ProtocolType::Kitty {
                let size = protocol_resize.size_for(&img, picker.font_size(), area.into());
                let img = protocol_resize.resize(&img, picker.font_size(), size, None);
                let kitty_id = next_kitty_id(&cache);
                let protocol = Kitty::new(img, size, kitty_id, std::env::var_os("TMUX").is_some())
                    .map(Protocol::Kitty);
                (protocol, Some(kitty_id))
            } else {
                (picker.new_protocol(img, area.into(), protocol_resize), None)
            };
            let protocol = match protocol {
                Ok(protocol) => protocol,
                Err(error) => {
                    tracing::warn!(%error, "failed to prepare artwork for the terminal");
                    cache.insert(key, None);
                    return false;
                }
            };
            cache.insert(key, Some(CachedProtocol { protocol, kitty_id }));
        }

        let Some(Some(cached)) = cache.get(&key) else {
            return false;
        };
        let render_area = match resize {
            ImageResize::Fit => area,
            ImageResize::Scale => centered_protocol_area(area, cached.protocol.size()),
        };
        f.render_widget(Image::new(&cached.protocol), render_area);
        true
    })
}

fn next_kitty_id(
    cache: &std::collections::HashMap<ProtocolCacheKey, Option<CachedProtocol>>,
) -> u32 {
    (KITTY_IMAGE_ID_START..KITTY_IMAGE_ID_START + PROTOCOL_CACHE_CAP as u32)
        .find(|candidate| {
            cache
                .values()
                .flatten()
                .all(|cached| cached.kitty_id != Some(*candidate))
        })
        .expect("protocol cache cap leaves a Kitty image ID available")
}

fn prune_superseded_scaled_protocols<T>(
    cache: &mut std::collections::HashMap<ProtocolCacheKey, Option<T>>,
    requested: ProtocolCacheKey,
) {
    if requested.resize == ImageResize::Scale {
        cache.retain(|existing, protocol| {
            existing.resize != ImageResize::Scale || *existing == requested || protocol.is_none()
        });
    }
}

pub(super) fn centered_protocol_area(area: Rect, size: Size) -> Rect {
    let width = size.width.min(area.width);
    let height = size.height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use ratatui_image::FontSize;

    #[test]
    fn scale_upfills_a_target_that_fit_leaves_at_natural_size() {
        let image = image::DynamicImage::new_rgba8(320, 320);
        let font_size = FontSize::new(8, 16);
        let target = Size::new(80, 40);

        assert_eq!(
            Resize::Fit(None).size_for(&image, font_size, target),
            Size::new(40, 20)
        );
        assert_eq!(
            Resize::Scale(None).size_for(&image, font_size, target),
            target
        );
    }

    #[test]
    fn actual_protocol_size_is_centered_after_cell_rounding() {
        assert_eq!(
            centered_protocol_area(Rect::new(10, 5, 80, 40), Size::new(76, 38)),
            Rect::new(12, 6, 76, 38),
        );
    }

    #[test]
    fn scaled_protocols_use_one_live_cache_slot_and_keep_failures() {
        fn key(content_hash: u64, resize: ImageResize) -> ProtocolCacheKey {
            ProtocolCacheKey {
                content_hash,
                width: 80,
                height: 40,
                resize,
            }
        }

        let fit = key(1, ImageResize::Fit);
        let old_scale = key(2, ImageResize::Scale);
        let new_scale = key(3, ImageResize::Scale);
        let failed_scale = key(4, ImageResize::Scale);
        let mut cache = std::collections::HashMap::from([
            (fit, Some(())),
            (old_scale, Some(())),
            (failed_scale, None),
        ]);

        prune_superseded_scaled_protocols(&mut cache, new_scale);
        cache.insert(new_scale, Some(()));

        assert!(cache.contains_key(&fit));
        assert!(!cache.contains_key(&old_scale));
        assert!(cache.contains_key(&new_scale));
        assert!(matches!(cache.get(&failed_scale), Some(None)));
        assert_eq!(
            cache
                .keys()
                .filter(|key| {
                    key.resize == ImageResize::Scale && cache.get(key).is_some_and(Option::is_some)
                })
                .count(),
            1,
        );
    }

    #[test]
    fn scaled_protocol_is_released_after_overlay_or_fullscreen_closes() {
        let art = |overlays| ImageFrameState {
            fullscreen_art: true,
            overlays,
        };
        let normal = ImageFrameState::default();
        let bare = art(Overlays::none());
        let help = art(Overlays::none().with(Overlays::HELP, true));
        let toast = art(Overlays::none().with(Overlays::STATUS, true));
        let both = art(Overlays::none()
            .with(Overlays::HELP, true)
            .with(Overlays::STATUS, true));

        assert!(!should_release_scaled_protocol(normal, bare));
        assert!(!should_release_scaled_protocol(bare, help));
        assert!(should_release_scaled_protocol(help, bare));
        assert!(should_release_scaled_protocol(bare, normal));
        // The toast outliving the modal is what a single bool could not see.
        assert!(should_release_scaled_protocol(both, toast));
        assert!(!should_release_scaled_protocol(toast, both));
    }

    #[test]
    fn invalid_image_failure_is_cached_and_reported() {
        let image = CachedImage::new(b"not an image".to_vec());
        let area = Rect::new(0, 0, 10, 5);
        let key = ProtocolCacheKey {
            content_hash: image.content_hash(),
            width: area.width,
            height: area.height,
            resize: ImageResize::Scale,
        };
        PROTOCOL_CACHE.with(|cache| cache.borrow_mut().clear());

        let mut rendered = true;
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        terminal
            .draw(|f| rendered = render_scaled_image(f, &image, area))
            .unwrap();

        assert!(!rendered);
        PROTOCOL_CACHE.with(|cache| {
            assert!(matches!(cache.borrow().get(&key), Some(None)));
            cache.borrow_mut().clear();
        });
    }
}
