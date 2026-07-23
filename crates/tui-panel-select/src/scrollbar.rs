//! Optional vertical scrollbar helpers, gated behind the `scrollbar` feature.
//!
//! Two pieces, both pure and panel-agnostic (they work for a
//! [`MultiSelectPanel`](crate::MultiSelectPanel), a
//! [`SelectablePanel`](crate::SelectablePanel), or any home-grown scrollable
//! content):
//!
//! - [`scroll_for_track_row`] maps a clicked/dragged terminal row within a
//!   scrollbar track to a scroll offset, so a click or drag anywhere in the
//!   track jumps/scrolls proportionally — the way a native scrollbar behaves.
//! - [`render_scrollbar`] draws a ratatui [`Scrollbar`] into a track column,
//!   sizing and positioning the thumb from `total`/`capacity`/`start` and
//!   no-op-ing when everything already fits.
//!
//! Panels that own their scroll offset (`MultiSelectPanel`) expose thin
//! convenience wrappers ([`MultiSelectPanel::scroll_to_track_row`] and
//! [`MultiSelectPanel::render_scrollbar`]) that plumb their own geometry into
//! these functions; callers that keep scroll externally can use the free
//! functions directly.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState, StatefulWidget};

/// Visual styling for [`render_scrollbar`]. Start from
/// [`ScrollbarStyle::default`] (a right-hand vertical bar with a `│` track and
/// a solid `█` thumb, both unstyled) and override what you want — most callers
/// only set `track_style`/`thumb_style` to their theme's dim/accent colours.
#[derive(Clone, Debug)]
pub struct ScrollbarStyle {
    /// Which edge the bar sits on and which way it runs.
    pub orientation: ScrollbarOrientation,
    /// The glyph drawn along the unfilled track (`None` leaves the cells as-is).
    pub track_symbol: Option<String>,
    /// The glyph drawn for the thumb (the draggable filled portion).
    pub thumb_symbol: String,
    /// Optional cap glyph at the track's start (arrow/corner); usually `None`.
    pub begin_symbol: Option<String>,
    /// Optional cap glyph at the track's end; usually `None`.
    pub end_symbol: Option<String>,
    /// Style (typically a dim foreground colour) for the track.
    pub track_style: Style,
    /// Style (typically an accent foreground colour) for the thumb.
    pub thumb_style: Style,
}

impl Default for ScrollbarStyle {
    fn default() -> Self {
        Self {
            orientation: ScrollbarOrientation::VerticalRight,
            track_symbol: Some("\u{2502}".to_string()),
            thumb_symbol: "\u{2588}".to_string(),
            begin_symbol: None,
            end_symbol: None,
            track_style: Style::default(),
            thumb_style: Style::default(),
        }
    }
}

/// Map a clicked/dragged terminal `row` within a vertical scrollbar `track`
/// to a scroll offset in `0..=max_scroll`, clamped to the track's own bounds
/// (so a click above the track reads as the top and below it as the bottom).
/// Returns `0` when there's nothing to scroll (an empty track or
/// `max_scroll == 0`).
///
/// ```
/// use ratatui::layout::Rect;
/// use tui_panel_select::scrollbar::scroll_for_track_row;
/// // An 11-row track at y=0, content that can scroll up to 100 rows.
/// let track = Rect::new(40, 0, 1, 11);
/// assert_eq!(scroll_for_track_row(track, 0, 100), 0); // top
/// assert_eq!(scroll_for_track_row(track, 10, 100), 100); // bottom
/// assert_eq!(scroll_for_track_row(track, 5, 100), 50); // middle
/// ```
pub fn scroll_for_track_row(track: Rect, row: u16, max_scroll: u16) -> u16 {
    if track.height == 0 || max_scroll == 0 {
        return 0;
    }
    let track_len = track.height.saturating_sub(1).max(1) as f64;
    let rel = row
        .saturating_sub(track.y)
        .min(track.height.saturating_sub(1)) as f64;
    (((rel / track_len) * max_scroll as f64).round() as u16).min(max_scroll)
}

/// Render a vertical scrollbar into `area`, sizing the thumb from `total`
/// content rows, the visible `capacity` (rows that fit at once) and the
/// current `start` scroll offset. A no-op when the area is empty or the
/// content already fits (`total <= capacity`), so callers can call it
/// unconditionally.
pub fn render_scrollbar(
    area: Rect,
    buf: &mut Buffer,
    total: usize,
    capacity: usize,
    start: usize,
    style: &ScrollbarStyle,
) {
    if area.width == 0 || area.height == 0 || total <= capacity {
        return;
    }
    let mut state = ScrollbarState::new(total - capacity).position(start);
    let bar = Scrollbar::new(style.orientation.clone())
        .begin_symbol(style.begin_symbol.as_deref())
        .end_symbol(style.end_symbol.as_deref())
        .track_symbol(style.track_symbol.as_deref())
        .thumb_symbol(style.thumb_symbol.as_str())
        .style(style.track_style)
        .thumb_style(style.thumb_style);
    StatefulWidget::render(bar, area, buf, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> Rect {
        // A one-column-wide, 11-row track anchored below the top border.
        Rect::new(40, 2, 1, 11)
    }

    #[test]
    fn track_row_maps_endpoints_and_midpoint() {
        let t = track();
        // Top of the track -> no scroll; bottom -> full scroll.
        assert_eq!(scroll_for_track_row(t, t.y, 100), 0);
        assert_eq!(scroll_for_track_row(t, t.y + t.height - 1, 100), 100);
        // Middle row (5 of 10 steps) -> half.
        assert_eq!(scroll_for_track_row(t, t.y + 5, 100), 50);
    }

    #[test]
    fn track_row_clamps_out_of_bounds_rows() {
        let t = track();
        // Above the track reads as the top, far below as the bottom.
        assert_eq!(scroll_for_track_row(t, 0, 100), 0);
        assert_eq!(scroll_for_track_row(t, 500, 100), 100);
    }

    #[test]
    fn track_row_is_zero_when_nothing_to_scroll() {
        assert_eq!(scroll_for_track_row(track(), 5, 0), 0);
        assert_eq!(scroll_for_track_row(Rect::new(0, 0, 1, 0), 5, 100), 0);
    }

    #[test]
    fn render_is_a_no_op_when_content_fits_or_area_empty() {
        let area = Rect::new(0, 0, 1, 10);
        let mut buf = Buffer::empty(area);
        // Fits: total <= capacity -> nothing drawn (buffer stays blank).
        render_scrollbar(area, &mut buf, 10, 10, 0, &ScrollbarStyle::default());
        let blank = Buffer::empty(area);
        assert_eq!(buf, blank);
        // Empty area -> also a no-op.
        render_scrollbar(
            Rect::new(0, 0, 0, 0),
            &mut buf,
            100,
            10,
            0,
            &ScrollbarStyle::default(),
        );
        assert_eq!(buf, blank);
    }

    #[test]
    fn render_draws_a_thumb_when_content_overflows() {
        let area = Rect::new(0, 0, 1, 10);
        let mut buf = Buffer::empty(area);
        render_scrollbar(area, &mut buf, 100, 10, 0, &ScrollbarStyle::default());
        // At least one cell must now carry the thumb glyph.
        let painted: String = (0..area.height)
            .map(|y| buf[(0, y)].symbol().to_string())
            .collect();
        assert!(
            painted.contains('\u{2588}'),
            "a scrollable panel must paint a thumb: {painted:?}"
        );
    }
}
