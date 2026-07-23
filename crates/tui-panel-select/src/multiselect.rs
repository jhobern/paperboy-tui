//! A batteries-included, multi-region text panel: [`MultiSelectPanel`].
//!
//! Where [`crate::panel::SelectablePanel`] covers the common single-selection,
//! mouse-only case, `MultiSelectPanel` bundles the same primitives into a panel
//! that additionally supports:
//!
//! - **Multiple simultaneous selection regions** — one *active* (live,
//!   draggable, keyboard-extendable) region plus any number of *finalized*
//!   ones, so an app can build up several highlighted runs (e.g. with an
//!   Alt-click gesture) and copy them together.
//! - **Keyboard extension** — grow the active region a character or a line at a
//!   time ([`extend`](MultiSelectPanel::extend)), scrolling it into view.
//! - **Owned scrolling with drag auto-scroll** — the panel owns its scroll
//!   offset, so a drag held past the top/bottom edge keeps scrolling and
//!   extending the selection ([`autoscroll_tick`](MultiSelectPanel::autoscroll_tick)).
//! - **Styled content** — set syntax-highlighted [`Line`]s directly
//!   ([`set_styled_content`](MultiSelectPanel::set_styled_content)), not just
//!   plain (or ANSI) text.
//!
//! Cross-*panel* concerns (ordering a copy that spans two different panels,
//! excluding app-specific annotation glyphs from copied text, drawing a
//! scrollbar) stay with the host: the panel exposes
//! [`selected_parts`](MultiSelectPanel::selected_parts) and
//! [`highlight_regions`](MultiSelectPanel::highlight_regions) so the host can
//! compose several panels however it likes.

use std::collections::HashSet;
use std::sync::Arc;

use ratatui::layout::Rect;
use ratatui::text::Line;

use crate::selection;
use crate::wrapcache::{PanelWrap, TextPos, WrapMarker, WrapMode};

/// A keyboard motion for [`MultiSelectPanel::extend`] — which way to move the
/// active region's live end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    /// One character left (crossing into the previous line at column 0).
    Left,
    /// One character right (crossing into the next line at its start).
    Right,
    /// One logical line up, keeping the column where possible.
    Up,
    /// One logical line down, keeping the column where possible.
    Down,
}

/// A vertical auto-scroll direction for
/// [`MultiSelectPanel::start_autoscroll`], used when a selection drag is held
/// past the panel's top or bottom edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoScroll {
    /// Scroll up (drag held above the panel).
    Up,
    /// Scroll down (drag held below the panel).
    Down,
}

/// One selection region: `anchor` is where it began, `cursor` its live end.
/// Stored as logical [`TextPos`] so it survives rewraps/resizes/scrolling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Region {
    anchor: TextPos,
    cursor: TextPos,
}

impl Region {
    /// The region's earliest position (its logical start), for ordering
    /// several regions by where they begin rather than by draw order.
    fn start(&self) -> TextPos {
        self.anchor.min(self.cursor)
    }
}

/// One scrollable text panel that owns its scroll offset and one-or-more
/// selection regions.
///
/// Cheap to keep across frames: the content setters only rebuild the wrap
/// cache when the content or width actually changed, and selections are logical
/// positions that are unaffected by rewrapping. See the [module
/// docs](self) for the feature set.
#[derive(Default)]
pub struct MultiSelectPanel {
    wrap: Option<PanelWrap>,
    /// How raw lines wider than the panel are laid out (wrap vs clip).
    mode: WrapMode,
    /// Optional end-of-row wrap marker (see [`WrapMarker`]).
    marker: Option<WrapMarker>,
    /// Current scroll offset, in wrapped rows.
    scroll: u16,
    /// The live region being dragged / keyboard-extended, if any.
    active: Option<Region>,
    /// Additional, already-finalized regions (e.g. Alt-click gestures).
    extras: Vec<Region>,
    /// While a drag is held past the top (`-1`) or bottom (`+1`) edge, the
    /// direction to keep auto-scrolling; `None` when the drag is inside.
    pending_autoscroll: Option<i8>,
}

impl MultiSelectPanel {
    /// A panel with no content, no selection and scroll at the top.
    pub fn new() -> Self {
        Self::default()
    }

    // --- Configuration ----------------------------------------------------

    /// Choose how raw lines wider than the panel are laid out. Takes effect on
    /// the next content set (which apps do every frame).
    pub fn set_wrap_mode(&mut self, mode: WrapMode) {
        self.mode = mode;
    }

    /// This panel's current [`WrapMode`].
    pub fn wrap_mode(&self) -> WrapMode {
        self.mode
    }

    /// Enable/disable the end-of-row wrap marker (see [`WrapMarker`]). Takes
    /// effect on the next content set.
    pub fn set_wrap_marker(&mut self, marker: Option<WrapMarker>) {
        self.marker = marker;
    }

    /// This panel's current end-of-row wrap marker, if any.
    pub fn wrap_marker(&self) -> Option<WrapMarker> {
        self.marker
    }

    // --- Content ----------------------------------------------------------

    /// Set (or update) the panel's plain text and inner wrap width. A no-op
    /// when neither the text (by `Arc` identity) nor the width nor the mode /
    /// marker changed — safe, and intended, to call every frame.
    pub fn set_content(&mut self, text: Arc<str>, width: usize) {
        PanelWrap::rebuild_if_needed_marker(&mut self.wrap, &text, width, self.mode, self.marker);
    }

    /// Set the panel's text from a string that may contain ANSI escape
    /// sequences: rendered rows keep their colour, while selection/copy/geometry
    /// operate on the plain, stripped text. Requires the `ansi` feature.
    #[cfg(feature = "ansi")]
    pub fn set_ansi_content(&mut self, text: Arc<str>, width: usize) {
        PanelWrap::rebuild_if_needed_ansi_marker(
            &mut self.wrap,
            &text,
            width,
            self.mode,
            self.marker,
        );
    }

    /// Set the panel's content from pre-styled [`Line`]s (e.g.
    /// syntax-highlighted source). Rendered rows keep their per-span styling
    /// while selection/copy/geometry operate on the plain text. Styled content
    /// has no stable identity to diff against, so this rebuilds every call —
    /// call it only when the content actually changed.
    pub fn set_styled_content(&mut self, lines: &[Line<'_>], width: usize) {
        self.wrap = Some(PanelWrap::build_styled_with_marker(
            lines,
            width,
            self.mode,
            self.marker,
        ));
    }

    /// Whether any content has been set yet.
    pub fn has_content(&self) -> bool {
        self.wrap.is_some()
    }

    /// The exact, unmodified source text the panel was built from — for a
    /// "copy the whole panel" action that needs no selection. `None` before any
    /// content is set.
    pub fn whole_text(&self) -> Option<&str> {
        self.wrap.as_ref().map(PanelWrap::source)
    }

    // --- Scrolling --------------------------------------------------------

    /// The total number of wrapped rows the current content occupies.
    pub fn total_rows(&self) -> u32 {
        self.wrap.as_ref().map_or(0, PanelWrap::total_rows)
    }

    /// The largest in-bounds scroll offset for a `viewport_height`-row window
    /// (content rows − height, floored at 0) — for sizing a scrollbar or
    /// clamping a scroll.
    pub fn max_scroll(&self, viewport_height: u16) -> u16 {
        let total = self.total_rows().min(u16::MAX as u32) as u16;
        total.saturating_sub(viewport_height)
    }

    /// This panel's current scroll offset, in wrapped rows.
    pub fn scroll(&self) -> u16 {
        self.scroll
    }

    /// Set the scroll offset directly (e.g. from a scrollbar drag). Not
    /// clamped here — call [`clamp_scroll`](Self::clamp_scroll) once the
    /// viewport height is known (at draw time).
    pub fn set_scroll(&mut self, scroll: u16) {
        self.scroll = scroll;
    }

    /// Move the scroll offset by `delta` rows (negative = up), clamped to
    /// `[0, max_scroll(viewport_height)]`.
    pub fn scroll_by(&mut self, delta: i32, viewport_height: u16) {
        let max = self.max_scroll(viewport_height) as i32;
        let next = (self.scroll as i32 + delta).clamp(0, max);
        self.scroll = next as u16;
    }

    /// Clamp the scroll offset into range for a `viewport_height`-row window
    /// and return that window's `max_scroll` (for the scrollbar). Call once per
    /// frame at draw time, after setting content.
    pub fn clamp_scroll(&mut self, viewport_height: u16) -> u16 {
        let max = self.max_scroll(viewport_height);
        self.scroll = self.scroll.min(max);
        max
    }

    // --- Rendering --------------------------------------------------------

    /// The visible wrapped rows for a `height`-row window at the current
    /// scroll — ready to render. Only on-screen rows are wrapped, regardless of
    /// total content size.
    pub fn visible_rows(&self, height: u16) -> Vec<Line<'static>> {
        self.wrap
            .as_ref()
            .map(|w| w.visible_window(self.scroll, height))
            .unwrap_or_default()
    }

    /// Every selection region's on-screen cells to highlight, as `(row,
    /// col_from, col_to_exclusive)` in absolute terminal coordinates, bounded
    /// to the visible window. Covers the active region and all finalized ones.
    pub fn highlight_regions(&self, area: Rect) -> Vec<(u16, u16, u16)> {
        let Some(wrap) = self.wrap.as_ref() else {
            return Vec::new();
        };
        let mut cells = Vec::new();
        for region in self.regions() {
            cells.extend(selection::highlight_cells(
                region.anchor,
                region.cursor,
                wrap,
                area,
                self.scroll,
            ));
        }
        cells
    }

    // --- Selection: mouse -------------------------------------------------

    /// Begin a new *active* selection region at terminal `point`, given the
    /// panel's inner `area`. Leaves any finalized ([`finalize_active`]) regions
    /// intact — call [`clear`](Self::clear) first for a fresh, single-region
    /// selection. No-op without content.
    ///
    /// [`finalize_active`]: Self::finalize_active
    pub fn begin(&mut self, area: Rect, point: (u16, u16)) {
        self.pending_autoscroll = None;
        let Some(wrap) = self.wrap.as_ref() else {
            self.active = None;
            return;
        };
        let pos = selection::point_to_textpos(point, area, self.scroll, wrap);
        self.active = Some(Region {
            anchor: pos,
            cursor: pos,
        });
    }

    /// Continue the active selection's drag to terminal `point`. When the drag
    /// moves past the panel's top/bottom edge this begins auto-scrolling in
    /// that direction (extending the selection a whole line at a time) and
    /// advances it once immediately; call
    /// [`autoscroll_tick`](Self::autoscroll_tick) from an idle loop to keep it
    /// going while the mouse is still. No-op without an active region.
    pub fn drag(&mut self, area: Rect, point: (u16, u16)) {
        if self.active.is_none() {
            return;
        }
        let (_, row) = point;
        if area.height > 0 && row < area.y {
            self.pending_autoscroll = Some(-1);
            self.autoscroll_tick(area);
            return;
        }
        if area.height > 0 && row >= area.y.saturating_add(area.height) {
            self.pending_autoscroll = Some(1);
            self.autoscroll_tick(area);
            return;
        }
        self.pending_autoscroll = None;
        let Some(wrap) = self.wrap.as_ref() else {
            return;
        };
        let pos = selection::point_to_textpos(point, area, self.scroll, wrap);
        if let Some(region) = self.active.as_mut() {
            region.cursor = pos;
        }
    }

    /// End the current drag (a mouse-up): stops any pending auto-scroll. The
    /// selection itself is kept — read it with
    /// [`selected_parts`](Self::selected_parts) / copy it as the host sees fit.
    pub fn end_drag(&mut self) {
        self.pending_autoscroll = None;
    }

    /// Whether a drag is currently held past an edge, waiting for
    /// [`autoscroll_tick`](Self::autoscroll_tick) to keep scrolling.
    pub fn has_pending_autoscroll(&self) -> bool {
        self.pending_autoscroll.is_some()
    }

    /// One "tick" of auto-scrolling a drag held past the panel's vertical
    /// bounds: scroll one row in the pending direction and extend the active
    /// region's live end to the newly revealed edge line. Once the content's
    /// own top/bottom is reached but the drag is still held past the edge, the
    /// cursor snaps to the very first/last line's full extent instead, so that
    /// boundary line ends up entirely highlighted. No-op when nothing is
    /// pending. `area` is the panel's inner rectangle.
    pub fn autoscroll_tick(&mut self, area: Rect) {
        let Some(dir) = self.pending_autoscroll else {
            return;
        };
        if self.active.is_none() {
            self.pending_autoscroll = None;
            return;
        }
        let max_scroll = self.max_scroll(area.height);
        let new_scroll = if dir < 0 {
            self.scroll.saturating_sub(1)
        } else {
            (self.scroll + 1).min(max_scroll)
        };
        let reached_bound = new_scroll == self.scroll;
        self.scroll = new_scroll;
        let Some(wrap) = self.wrap.as_ref() else {
            return;
        };
        let edge_row = if reached_bound {
            if dir < 0 {
                0
            } else {
                wrap.total_rows().saturating_sub(1)
            }
        } else if dir < 0 {
            new_scroll as u32
        } else {
            (new_scroll as u32 + area.height as u32).saturating_sub(1)
        };
        let col = if dir < 0 { 0 } else { usize::MAX };
        let pos = wrap.row_col_to_textpos(edge_row, col);
        if let Some(region) = self.active.as_mut() {
            region.cursor = pos;
        }
    }

    // --- Selection: keyboard ---------------------------------------------

    /// Move the active region's live end by one character ([`Motion::Left`] /
    /// [`Motion::Right`], crossing line boundaries) or one logical line
    /// ([`Motion::Up`] / [`Motion::Down`], keeping the column where possible),
    /// then scroll the panel so that end stays visible. No-op without an active
    /// region or content. `area` is the panel's inner rectangle.
    pub fn extend(&mut self, motion: Motion, area: Rect) {
        let Some(region) = self.active else {
            return;
        };
        let Some(wrap) = self.wrap.as_ref() else {
            return;
        };
        let mut pos = region.cursor;
        match motion {
            Motion::Left => {
                if pos.col > 0 {
                    pos.col -= 1;
                } else if pos.line > 0 {
                    pos.line -= 1;
                    pos.col = wrap.line_char_len(pos.line).saturating_sub(1);
                }
            }
            Motion::Right => {
                let len = wrap.line_char_len(pos.line);
                if pos.col + 1 < len {
                    pos.col += 1;
                } else if pos.line + 1 < wrap.line_count() {
                    pos.line += 1;
                    pos.col = 0;
                }
            }
            Motion::Up => {
                if pos.line > 0 {
                    pos.line -= 1;
                    pos.col = pos.col.min(wrap.line_char_len(pos.line).saturating_sub(1));
                }
            }
            Motion::Down => {
                if pos.line + 1 < wrap.line_count() {
                    pos.line += 1;
                    pos.col = pos.col.min(wrap.line_char_len(pos.line).saturating_sub(1));
                }
            }
        }
        if let Some(region) = self.active.as_mut() {
            region.cursor = pos;
        }
        self.scroll_cursor_into_view(area);
    }

    /// After moving the active region's live end, nudge the scroll so that end
    /// stays visible, like a text editor never letting its cursor scroll off
    /// screen.
    fn scroll_cursor_into_view(&mut self, area: Rect) {
        if area.height == 0 {
            return;
        }
        let Some(region) = self.active else {
            return;
        };
        let Some(wrap) = self.wrap.as_ref() else {
            return;
        };
        let (row, _) = wrap.textpos_to_row_col(region.cursor);
        let max_scroll = self.max_scroll(area.height);
        if row < self.scroll as u32 {
            self.scroll = row as u16;
        } else if row >= self.scroll as u32 + area.height as u32 {
            self.scroll = (row + 1).saturating_sub(area.height as u32) as u16;
        }
        self.scroll = self.scroll.min(max_scroll);
    }

    // --- Selection: regions & text ---------------------------------------

    /// Finalize the active region: move it into the set of kept regions and
    /// clear the live one, so a subsequent [`begin`](Self::begin) starts a new
    /// region alongside it. No-op when there's no active region.
    pub fn finalize_active(&mut self) {
        if let Some(region) = self.active.take() {
            self.extras.push(region);
        }
        self.pending_autoscroll = None;
    }

    /// Drop every selection region (active and finalized) and stop any pending
    /// auto-scroll. Call whenever the underlying content is about to change so
    /// a highlight never lingers over stale text.
    pub fn clear(&mut self) {
        self.active = None;
        self.extras.clear();
        self.pending_autoscroll = None;
    }

    /// Whether there is any selection region at all (active or finalized).
    pub fn has_selection(&self) -> bool {
        self.active.is_some() || !self.extras.is_empty()
    }

    /// The panel's wrap cache, if content has been set. Exposed so a host can
    /// run its own geometry queries (row/column ↔ [`TextPos`] mapping, line
    /// text, hit-testing) against the exact layout the panel is rendering.
    pub fn wrap(&self) -> Option<&PanelWrap> {
        self.wrap.as_ref()
    }

    /// The live (active) region as `(anchor, cursor)` logical positions, or
    /// `None` when nothing is being dragged / keyboard-extended. `anchor` is
    /// where the region began; `cursor` is its live end.
    pub fn active_selection(&self) -> Option<(TextPos, TextPos)> {
        self.active.map(|r| (r.anchor, r.cursor))
    }

    /// Replace the live (active) region with one spanning `anchor`..`cursor`,
    /// without touching any finalized regions. Lets a host restore or script a
    /// selection (the positions are logical, so they survive rewraps).
    pub fn set_active_selection(&mut self, anchor: TextPos, cursor: TextPos) {
        self.active = Some(Region { anchor, cursor });
    }

    /// The finalized regions (those moved aside by
    /// [`finalize_active`](Self::finalize_active)) as `(anchor, cursor)` pairs,
    /// in insertion order.
    pub fn finalized_selections(&self) -> Vec<(TextPos, TextPos)> {
        self.extras.iter().map(|r| (r.anchor, r.cursor)).collect()
    }

    /// Append a finalized region spanning `anchor`..`cursor`, as though it had
    /// been dragged and then [`finalize_active`](Self::finalize_active)d. Lets
    /// a host restore several kept regions.
    pub fn push_finalized(&mut self, anchor: TextPos, cursor: TextPos) {
        self.extras.push(Region { anchor, cursor });
    }

    /// Begin auto-scrolling in `dir` on the next
    /// [`autoscroll_tick`](Self::autoscroll_tick), as though a drag were being
    /// held past that edge. Mainly for hosts/tests that drive auto-scroll
    /// without simulating exact drag geometry.
    pub fn start_autoscroll(&mut self, dir: AutoScroll) {
        self.pending_autoscroll = Some(match dir {
            AutoScroll::Up => -1,
            AutoScroll::Down => 1,
        });
    }

    /// All regions (finalized then active), for iterating in insertion order.
    fn regions(&self) -> impl Iterator<Item = &Region> {
        self.extras.iter().chain(self.active.iter())
    }

    /// The extracted text of every selection region, **ordered by where each
    /// region starts** in the content (not by draw order), each as one element.
    /// Empty (whitespace-only) regions are skipped. `exclude` drops individual
    /// character positions from the copied text (e.g. app-specific annotation
    /// glyphs). The host joins these — possibly across several panels — however
    /// it wants (see [`selected_text`](Self::selected_text) for the common
    /// single-panel join).
    pub fn selected_parts(&self, exclude: Option<&HashSet<TextPos>>) -> Vec<String> {
        let Some(wrap) = self.wrap.as_ref() else {
            return Vec::new();
        };
        let mut regions: Vec<&Region> = self.regions().collect();
        regions.sort_by_key(|r| r.start());
        let mut parts = Vec::new();
        for region in regions {
            if let Some(text) = selection::extract_text(region.anchor, region.cursor, wrap, exclude)
            {
                parts.push(text);
            }
        }
        parts
    }

    /// The whole selection as a single string: every region's text (ordered by
    /// start) joined by a blank line, or `None` when nothing is selected. A
    /// convenience over [`selected_parts`](Self::selected_parts) for the common
    /// single-panel case.
    pub fn selected_text(&self, exclude: Option<&HashSet<TextPos>>) -> Option<String> {
        let parts = self.selected_parts(exclude);
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        }
    }
}

/// Scrollbar conveniences (default-on `scrollbar` feature): thin wrappers that
/// plumb the panel's own geometry into the panel-agnostic
/// [`crate::scrollbar`] helpers.
#[cfg(feature = "scrollbar")]
impl MultiSelectPanel {
    /// Jump/scroll to the position a scrollbar-track click or drag at terminal
    /// `row` maps to, given the `track` Rect (the panel's scrollbar column).
    /// The track's height doubles as the viewport height for computing the
    /// scrollable extent, so this stays correct between frames without any
    /// cached `max_scroll`.
    pub fn scroll_to_track_row(&mut self, track: ratatui::layout::Rect, row: u16) {
        let max = self.max_scroll(track.height);
        self.set_scroll(crate::scrollbar::scroll_for_track_row(track, row, max));
    }

    /// Render this panel's vertical scrollbar into `area` (its scrollbar
    /// column) with `style`. A no-op when the content already fits, so it's
    /// safe to call every frame; `area.height` is taken as the visible row
    /// capacity.
    pub fn render_scrollbar(
        &self,
        area: ratatui::layout::Rect,
        buf: &mut ratatui::buffer::Buffer,
        style: &crate::scrollbar::ScrollbarStyle,
    ) {
        crate::scrollbar::render_scrollbar(
            area,
            buf,
            self.total_rows() as usize,
            area.height as usize,
            self.scroll() as usize,
            style,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel(text: &str, width: usize) -> MultiSelectPanel {
        let mut p = MultiSelectPanel::new();
        p.set_content(Arc::from(text), width);
        p
    }

    // A 40-wide, 10-high panel anchored at the origin.
    fn area() -> Rect {
        Rect::new(0, 0, 40, 10)
    }

    #[test]
    fn mouse_drag_selects_a_run_within_one_line() {
        let mut p = panel("hello world", 40);
        p.begin(area(), (0, 0)); // 'h'
        p.drag(area(), (4, 0)); // through 'hello' (inclusive of col 4)
        assert_eq!(p.selected_text(None).as_deref(), Some("hello"));
        assert!(p.has_selection());
    }

    #[test]
    fn clear_drops_all_regions() {
        let mut p = panel("hello world", 40);
        p.begin(area(), (0, 0));
        p.drag(area(), (5, 0));
        p.clear();
        assert!(!p.has_selection());
        assert_eq!(p.selected_text(None), None);
    }

    #[test]
    fn multiple_regions_copy_in_start_order_regardless_of_creation_order() {
        // Two lines; select the SECOND line's word first, then the FIRST.
        let mut p = panel("alpha\nbravo", 40);
        // Region A: "bravo" on line 1.
        p.begin(area(), (0, 1));
        p.drag(area(), (5, 1));
        p.finalize_active();
        // Region B: "alpha" on line 0.
        p.begin(area(), (0, 0));
        p.drag(area(), (5, 0));
        // Ordered by start -> alpha (line 0) before bravo (line 1).
        assert_eq!(p.selected_parts(None), vec!["alpha", "bravo"]);
        assert_eq!(p.selected_text(None).as_deref(), Some("alpha\n\nbravo"));
    }

    #[test]
    fn keyboard_extend_grows_the_active_region() {
        let mut p = panel("hello world", 40);
        p.begin(area(), (0, 0)); // caret at 'h', empty selection
        p.extend(Motion::Right, area());
        p.extend(Motion::Right, area());
        // anchor col 0 ..= cursor col 2 -> "hel"
        assert_eq!(p.selected_text(None).as_deref(), Some("hel"));
    }

    #[test]
    fn drag_past_bottom_edge_autoscrolls_and_extends() {
        // 30 short lines into a 10-row window: dragging below the panel
        // scrolls down and keeps extending the selection.
        let body: String = (0..30)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut p = panel(&body, 40);
        p.begin(area(), (0, 0)); // start at top
        assert_eq!(p.scroll(), 0);
        // Drag below the panel's bottom edge (area.y + height = 10).
        p.drag(area(), (0, 10));
        assert!(p.has_pending_autoscroll());
        assert!(p.scroll() > 0, "auto-scrolled down");
        // Idle ticks keep scrolling toward the bottom.
        for _ in 0..40 {
            p.autoscroll_tick(area());
        }
        assert_eq!(p.scroll(), p.max_scroll(area().height));
        // The selection now reaches the last line.
        let text = p.selected_text(None).unwrap();
        assert!(text.starts_with("line0"));
        assert!(text.contains("line29"));
    }

    #[test]
    fn end_drag_stops_autoscroll_but_keeps_the_selection() {
        let body: String = (0..30)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut p = panel(&body, 40);
        p.begin(area(), (0, 0));
        p.drag(area(), (0, 10));
        assert!(p.has_pending_autoscroll());
        p.end_drag();
        assert!(!p.has_pending_autoscroll());
        assert!(p.has_selection());
    }

    #[test]
    fn scroll_helpers_clamp_to_content() {
        let body: String = (0..30)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut p = panel(&body, 40);
        assert_eq!(p.total_rows(), 30);
        assert_eq!(p.max_scroll(10), 20);
        p.set_scroll(999);
        assert_eq!(p.clamp_scroll(10), 20);
        assert_eq!(p.scroll(), 20);
        p.scroll_by(-5, 10);
        assert_eq!(p.scroll(), 15);
        p.scroll_by(100, 10);
        assert_eq!(p.scroll(), 20);
    }

    #[test]
    fn exclude_drops_specific_positions_from_copied_text() {
        let mut p = panel("a!bc", 40);
        p.begin(area(), (0, 0));
        p.drag(area(), (4, 0));
        let mut ex = HashSet::new();
        ex.insert(TextPos::new(0, 1)); // the '!'
        assert_eq!(p.selected_text(Some(&ex)).as_deref(), Some("abc"));
    }

    #[test]
    fn styled_content_selects_on_the_plain_text() {
        use ratatui::style::{Color, Style};
        let lines = vec![Line::from(vec![
            ratatui::text::Span::styled("key", Style::default().fg(Color::Green)),
            ratatui::text::Span::raw(": v"),
        ])];
        let mut p = MultiSelectPanel::new();
        p.set_styled_content(&lines, 40);
        p.begin(area(), (0, 0));
        p.drag(area(), (2, 0)); // "key" (inclusive of col 2)
        assert_eq!(p.selected_text(None).as_deref(), Some("key"));
        // Rendered row keeps the colour.
        let rows = p.visible_rows(10);
        assert_eq!(rows[0].spans[0].style.fg, Some(Color::Green));
    }

    #[test]
    fn active_and_finalized_selections_round_trip_programmatically() {
        let mut p = panel("alpha\nbravo", 40);
        assert_eq!(p.active_selection(), None);
        assert!(p.finalized_selections().is_empty());

        p.set_active_selection(TextPos::new(0, 0), TextPos::new(0, 4));
        assert_eq!(
            p.active_selection(),
            Some((TextPos::new(0, 0), TextPos::new(0, 4)))
        );
        // Inclusive of the cursor column -> "alpha".
        assert_eq!(p.selected_text(None).as_deref(), Some("alpha"));

        p.push_finalized(TextPos::new(1, 0), TextPos::new(1, 4));
        assert_eq!(
            p.finalized_selections(),
            vec![(TextPos::new(1, 0), TextPos::new(1, 4))]
        );
        assert_eq!(p.selected_parts(None), vec!["alpha", "bravo"]);
    }

    #[test]
    fn wrap_accessor_exposes_the_live_layout() {
        let p = panel("hello world", 40);
        let wrap = p.wrap().expect("content was set");
        assert_eq!(wrap.line_text(0), "hello world");
    }

    #[test]
    fn start_autoscroll_drives_autoscroll_tick() {
        let body: String = (0..30)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut p = panel(&body, 40);
        p.set_active_selection(TextPos::new(0, 0), TextPos::new(0, 0));
        p.start_autoscroll(AutoScroll::Down);
        assert!(p.has_pending_autoscroll());
        p.autoscroll_tick(area());
        assert_eq!(p.scroll(), 1);
    }

    #[test]
    fn wrap_marker_is_drawn_on_wrapped_rows_but_not_the_last() {
        // Width 4 with a reserved marker column -> wraps at 3 chars.
        let mut p = MultiSelectPanel::new();
        p.set_wrap_marker(Some(WrapMarker::default()));
        p.set_content(Arc::from("abcdef"), 4);
        assert_eq!(p.wrap_marker(), Some(WrapMarker::default()));

        let rows = p.visible_rows(10);
        assert_eq!(rows.len(), 2, "'abcdef' wraps to two rows at width 3");
        // The first (continued) row ends with the marker glyph...
        let first_last = &rows[0].spans[rows[0].spans.len() - 1];
        assert_eq!(first_last.content, WrapMarker::default().glyph.to_string());
        // ...the final row does not (nothing continues after it).
        let second_last = &rows[1].spans[rows[1].spans.len() - 1];
        assert_ne!(second_last.content, WrapMarker::default().glyph.to_string());

        // The marker column is purely visual: it never maps to a character,
        // so selecting the whole logical line still yields exactly the text.
        p.set_active_selection(TextPos::new(0, 0), TextPos::new(0, 5));
        assert_eq!(p.selected_text(None).as_deref(), Some("abcdef"));
    }

    #[test]
    fn wrap_marker_survives_styled_content() {
        use ratatui::text::Span;
        let mut p = MultiSelectPanel::new();
        p.set_wrap_marker(Some(WrapMarker::default()));
        let lines = vec![Line::from(vec![Span::raw("abcdef")])];
        p.set_styled_content(&lines, 4);
        let rows = p.visible_rows(10);
        assert_eq!(rows.len(), 2);
        let first_last = &rows[0].spans[rows[0].spans.len() - 1];
        assert_eq!(first_last.content, WrapMarker::default().glyph.to_string());
    }

    #[cfg(feature = "scrollbar")]
    #[test]
    fn scroll_to_track_row_maps_a_click_to_the_panels_scroll() {
        // 40 one-char rows in a 10-row viewport -> max_scroll 30.
        let body: String = (0..40).map(|i| format!("line{i}\n")).collect();
        let mut p = panel(&body, 40);
        // The scrollbar track is 10 rows tall (the viewport height).
        let track = Rect::new(39, 0, 1, 10);
        // A click at the very bottom of the track jumps to max scroll.
        p.scroll_to_track_row(track, 9);
        assert_eq!(p.scroll(), p.max_scroll(10));
        // A click at the top returns to zero.
        p.scroll_to_track_row(track, 0);
        assert_eq!(p.scroll(), 0);
    }

    #[cfg(feature = "scrollbar")]
    #[test]
    fn render_scrollbar_paints_a_thumb_only_when_content_overflows() {
        use crate::scrollbar::ScrollbarStyle;
        use ratatui::buffer::Buffer;

        let long: String = (0..40).map(|i| format!("line{i}\n")).collect();
        let mut p = panel(&long, 40);
        p.clamp_scroll(10);
        let area = Rect::new(0, 0, 1, 10);
        let mut buf = Buffer::empty(area);
        p.render_scrollbar(area, &mut buf, &ScrollbarStyle::default());
        let painted: String = (0..area.height)
            .map(|y| buf[(0, y)].symbol().to_string())
            .collect();
        assert!(
            painted.contains('\u{2588}'),
            "overflowing content shows a thumb"
        );

        // A panel whose content fits draws nothing.
        let mut short = panel("only one line", 40);
        short.clamp_scroll(10);
        let mut blank = Buffer::empty(area);
        short.render_scrollbar(area, &mut blank, &ScrollbarStyle::default());
        assert_eq!(blank, Buffer::empty(area));
    }
}
