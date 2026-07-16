//! Shared line/wrap-structure cache backing both the Request JSON and
//! Response panels' rendering *and* their text selection.
//!
//! A panel's underlying text (an HTTP response body, a JSON request preview)
//! is split once into raw (unwrapped) lines and their wrapped-row extents —
//! not on every redraw — so scrolling/dragging a selection over an
//! "obscenely large" body costs only what's on screen, never the whole
//! body (see `rebuild_if_needed`/`visible_window`). The same structure also
//! converts between *screen* space (a wrapped row/col, valid only for the
//! current frame's scroll + panel width) and *logical* space (a raw line
//! index + character offset, stable across resizes/rewraps/rescrolls) —
//! which is what lets a selection survive a panel resize by staying on the
//! same characters instead of the same terminal coordinates.

use std::cell::RefCell;
use std::sync::Arc;

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::wrap::{wrap_line, wrap_line_window, wrapped_row_count};

/// How a panel lays out raw lines wider than its inner width.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WrapMode {
    /// Break each raw line every `width` columns onto as many rows as needed
    /// (the default). One raw line may occupy several screen rows.
    #[default]
    Wrap,
    /// Render each raw line on exactly one screen row, clipping anything past
    /// the panel's right edge — no wrapping and no horizontal scroll. One raw
    /// line always maps to exactly one row, which is what a panel that
    /// displays pre-formatted, column-aligned output (e.g. program output
    /// echoed verbatim) wants.
    Clip,
}

/// A position in a panel's logical (unwrapped) text: which raw line
/// (0-based), and which character offset within it (0-based; may equal the
/// line's own length to mean "just past its last character"). Deliberately
/// never a screen/terminal coordinate, so it stays valid across rewraps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextPos {
    pub line: usize,
    pub col: usize,
}

impl TextPos {
    pub fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

/// Per-line style runs `(char_from, char_to_exclusive, style)`, one inner
/// `Vec` per raw line, aligned to the plain text's characters. Only populated
/// for ANSI content (the `ansi` feature); `None` means "render unstyled".
type LineStyles = Vec<Vec<(usize, usize, Style)>>;

/// Exclusive prefix sum of wrapped-row counts across a panel's raw lines:
/// `cum[i]` = total wrapped rows in lines `0..i`. `cum.len() == line_count +
/// 1`; `*cum.last()` is the grand total (0 for no lines at all). Also caches
/// each line's own character length (`lens`) — computed once here, from the
/// same pass that already has to walk every line to determine wrapped-row
/// counts — so `PanelWrap::line_char_len` never has to re-scan a line's
/// characters itself (an O(1) selection/highlight primitive, even for a
/// single enormous line).
struct LineRows {
    cum: Vec<u32>,
    lens: Vec<usize>,
}

impl LineRows {
    fn build(char_lens: impl Iterator<Item = usize>, width: usize, mode: WrapMode) -> Self {
        let mut cum = vec![0u32];
        let mut lens = Vec::new();
        let mut total = 0u32;
        for len in char_lens {
            let rows = match mode {
                WrapMode::Wrap => wrapped_row_count(len, width) as u32,
                // Clip mode collapses every raw line onto a single row.
                WrapMode::Clip => 1,
            };
            total += rows;
            cum.push(total);
            lens.push(len);
        }
        Self { cum, lens }
    }

    fn total_rows(&self) -> u32 {
        (*self.cum.last().unwrap_or(&0)).max(1)
    }

    fn line_count(&self) -> usize {
        self.cum.len().saturating_sub(1)
    }

    /// The raw line index and row-offset-within-that-line for absolute
    /// wrapped row `row`, found by binary search (not a linear scan) so
    /// locating a scroll position deep into a huge body stays cheap.
    fn locate(&self, row: u32) -> (usize, u32) {
        if self.cum.len() <= 1 {
            return (0, 0);
        }
        // First index whose cumulative count exceeds `row`; the line just
        // before it is the one containing `row`.
        let idx = self.cum.partition_point(|&c| c <= row);
        let line = idx.saturating_sub(1).min(self.cum.len() - 2);
        (line, row - self.cum[line])
    }
}

/// Cached line/wrap structure for one panel's text, rebuilt only when its
/// content or width actually changes (see [`PanelWrap::rebuild_if_needed`]).
pub struct PanelWrap {
    /// The exact text (or, in ANSI mode, the raw text *with* escape
    /// sequences) this cache was built from — kept for a cheap `Arc::ptr_eq`
    /// "has the content changed?" check. In plain mode this is the same `Arc`
    /// as [`source`](Self::source); in ANSI mode it's the un-stripped input.
    raw: Arc<str>,
    /// The plain (ANSI-stripped) text that all geometry, selection and copy
    /// operate on — kept alive so `line_ranges` (byte offsets into it) stay
    /// valid.
    source: Arc<str>,
    /// Byte (start, end) of each raw line within `source` (split on '\n',
    /// stripping a trailing '\r', matching `str::lines()`).
    line_ranges: Vec<(usize, usize)>,
    rows: LineRows,
    width: usize,
    mode: WrapMode,
    /// Per-line style runs `(char_from, char_to_exclusive, style)` for ANSI
    /// content, aligned to `source`'s characters; `None` for plain text
    /// (rendered without styling). Only ever populated via the `ansi`
    /// feature.
    line_styles: Option<LineStyles>,
    /// The last `visible_window` result, keyed by the `(scroll, height)` it
    /// was computed for. Most frames redraw with an unchanged scroll
    /// position, so this turns those into an O(1) clone of a handful of
    /// already-wrapped rows instead of re-wrapping anything — no per-frame
    /// work proportional to content size, no matter how large the body or
    /// how long an individual line is.
    last_window: RefCell<Option<(u16, u16, Vec<Line<'static>>)>>,
}

impl PanelWrap {
    /// Build fresh from `source` at `width` columns, wrapping long lines
    /// ([`WrapMode::Wrap`]). O(source length) — call only when content/width
    /// has actually changed (see `rebuild_if_needed`), never unconditionally
    /// on every frame.
    pub fn build(source: Arc<str>, width: usize) -> Self {
        Self::build_with(source, width, WrapMode::Wrap)
    }

    /// Build fresh from plain `source` with an explicit [`WrapMode`].
    pub fn build_with(source: Arc<str>, width: usize, mode: WrapMode) -> Self {
        let line_ranges = Self::split_line_ranges(&source);
        let rows = LineRows::build(
            line_ranges
                .iter()
                .map(|&(s, e)| source[s..e].chars().count()),
            width,
            mode,
        );
        Self {
            raw: Arc::clone(&source),
            source,
            line_ranges,
            rows,
            width,
            mode,
            line_styles: None,
            last_window: RefCell::new(None),
        }
    }

    /// Split `source` into raw-line byte ranges (on '\n', dropping a trailing
    /// '\r'), matching `str::lines()`.
    fn split_line_ranges(source: &str) -> Vec<(usize, usize)> {
        let mut line_ranges = Vec::new();
        let bytes = source.as_bytes();
        let mut start = 0usize;
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'\n' {
                let mut end = i;
                if end > start && bytes[end - 1] == b'\r' {
                    end -= 1;
                }
                line_ranges.push((start, end));
                start = i + 1;
            }
        }
        if start < bytes.len() || line_ranges.is_empty() {
            line_ranges.push((start, bytes.len()));
        }
        line_ranges
    }

    /// Build fresh from ANSI-coloured `raw` with an explicit [`WrapMode`]. The
    /// escape sequences are parsed once into per-line style runs; all
    /// geometry, selection and copy operate on the plain, stripped text, so
    /// colour is purely a rendering concern. Requires the `ansi` feature.
    #[cfg(feature = "ansi")]
    pub fn build_ansi(raw: Arc<str>, width: usize, mode: WrapMode) -> Self {
        let (plain_lines, styles) = parse_ansi(&raw);
        let source: Arc<str> = Arc::from(plain_lines.join("\n"));
        // Byte ranges built directly from the plain lines we just produced, so
        // they stay exactly aligned with `styles` (one entry per line).
        let mut line_ranges = Vec::with_capacity(plain_lines.len().max(1));
        let mut pos = 0usize;
        for line in &plain_lines {
            let start = pos;
            let end = start + line.len();
            line_ranges.push((start, end));
            pos = end + 1; // skip the '\n' the join inserts
        }
        if line_ranges.is_empty() {
            line_ranges.push((0, 0));
        }
        let rows = LineRows::build(plain_lines.iter().map(|l| l.chars().count()), width, mode);
        Self {
            raw,
            source,
            line_ranges,
            rows,
            width,
            mode,
            line_styles: Some(styles),
            last_window: RefCell::new(None),
        }
    }

    /// Rebuild only if `source`'s identity (by pointer — a new response/edit
    /// always produces a fresh allocation) or `width` differ from what's
    /// cached; otherwise this is a no-op, keeping repeated frames (drags,
    /// idle redraws) cheap regardless of how large the content is. Plain
    /// text, [`WrapMode::Wrap`].
    pub fn rebuild_if_needed(cache: &mut Option<PanelWrap>, source: &Arc<str>, width: usize) {
        Self::rebuild_if_needed_with(cache, source, width, WrapMode::Wrap);
    }

    /// Like [`rebuild_if_needed`](Self::rebuild_if_needed) but for plain text
    /// with an explicit [`WrapMode`]. Also rebuilds if the mode changed, or if
    /// the cache currently holds ANSI-styled content.
    pub fn rebuild_if_needed_with(
        cache: &mut Option<PanelWrap>,
        source: &Arc<str>,
        width: usize,
        mode: WrapMode,
    ) {
        let stale = match cache {
            Some(c) => {
                !Arc::ptr_eq(&c.raw, source)
                    || c.width != width
                    || c.mode != mode
                    || c.line_styles.is_some()
            }
            None => true,
        };
        if stale {
            *cache = Some(PanelWrap::build_with(Arc::clone(source), width, mode));
        }
    }

    /// Like [`rebuild_if_needed_with`](Self::rebuild_if_needed_with) but for
    /// ANSI-coloured content. Also rebuilds if the mode changed, or if the
    /// cache currently holds plain content. Requires the `ansi` feature.
    #[cfg(feature = "ansi")]
    pub fn rebuild_if_needed_ansi(
        cache: &mut Option<PanelWrap>,
        raw: &Arc<str>,
        width: usize,
        mode: WrapMode,
    ) {
        let stale = match cache {
            Some(c) => {
                !Arc::ptr_eq(&c.raw, raw)
                    || c.width != width
                    || c.mode != mode
                    || c.line_styles.is_none()
            }
            None => true,
        };
        if stale {
            *cache = Some(PanelWrap::build_ansi(Arc::clone(raw), width, mode));
        }
    }

    /// This panel's line-layout mode.
    pub fn mode(&self) -> WrapMode {
        self.mode
    }

    pub fn line_count(&self) -> usize {
        self.rows.line_count()
    }

    /// The exact, unmodified text this cache was built from — every line,
    /// with its original line endings, not just what's currently scrolled
    /// into view. Used for "copy the whole panel" (no selection needed).
    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn line_text(&self, idx: usize) -> &str {
        let (s, e) = self.line_ranges[idx];
        &self.source[s..e]
    }

    pub fn line_char_len(&self, idx: usize) -> usize {
        self.rows.lens.get(idx).copied().unwrap_or(0)
    }

    pub fn total_rows(&self) -> u32 {
        self.rows.total_rows()
    }

    /// The exact wrapped rows visible in a `height`-row window starting at
    /// absolute wrapped-row `scroll` — the only rows actually wrapped, and
    /// only the portion of each raw line that window actually covers
    /// (`wrap_line_window`), regardless of the total content size or how
    /// long any single raw line is. Repeated calls with the same
    /// `(scroll, height)` (the common case across idle/unchanged frames)
    /// hit `last_window` and do no wrapping work at all.
    pub fn visible_window(&self, scroll: u16, height: u16) -> Vec<Line<'static>> {
        if height == 0 || self.line_count() == 0 {
            return Vec::new();
        }
        if let Some((cached_scroll, cached_height, cached)) = self.last_window.borrow().as_ref()
            && *cached_scroll == scroll
            && *cached_height == height
        {
            return cached.clone();
        }
        let out = match self.mode {
            WrapMode::Clip => self.visible_window_clip(scroll, height),
            WrapMode::Wrap => self.visible_window_wrap(scroll, height),
        };
        *self.last_window.borrow_mut() = Some((scroll, height, out.clone()));
        out
    }

    /// [`WrapMode::Wrap`] window: wrap only the rows actually on screen.
    fn visible_window_wrap(&self, scroll: u16, height: u16) -> Vec<Line<'static>> {
        let (start_line, row_in_line) = self.rows.locate(scroll as u32);
        let height_usize = height as usize;
        let mut out: Vec<Line<'static>> = Vec::with_capacity(height_usize);
        let mut skip = row_in_line as usize;
        for idx in start_line..self.line_count() {
            if out.len() >= height_usize {
                break;
            }
            let budget = height_usize - out.len();
            if self.line_styles.is_none() {
                out.extend(wrap_line_window(
                    self.line_text(idx),
                    self.width,
                    skip,
                    budget,
                ));
            } else {
                out.extend(self.wrap_line_window_styled(idx, skip, budget));
            }
            skip = 0;
        }
        out.truncate(height_usize);
        out
    }

    /// [`WrapMode::Clip`] window: one row per raw line, each clipped to
    /// `width` characters (so a single enormous line still costs only what's
    /// on screen).
    fn visible_window_clip(&self, scroll: u16, height: u16) -> Vec<Line<'static>> {
        let start = scroll as usize;
        let height_usize = height as usize;
        let mut out: Vec<Line<'static>> = Vec::with_capacity(height_usize);
        for idx in start..self.line_count() {
            if out.len() >= height_usize {
                break;
            }
            let end = self.line_char_len(idx).min(self.width);
            out.push(Line::from(self.styled_spans(idx, 0, end)));
        }
        out
    }

    /// Wrap only a bounded window of a *styled* line: skip `skip_rows` whole
    /// wrapped rows, then wrap at most `max_rows` more — without materialising
    /// the rest of the line (the styled counterpart of `wrap_line_window`).
    fn wrap_line_window_styled(
        &self,
        idx: usize,
        skip_rows: usize,
        max_rows: usize,
    ) -> Vec<Line<'static>> {
        if max_rows == 0 {
            return Vec::new();
        }
        if self.width == 0 {
            return if skip_rows == 0 {
                vec![Line::from(self.styled_spans(
                    idx,
                    0,
                    self.line_char_len(idx),
                ))]
            } else {
                Vec::new()
            };
        }
        let c0 = skip_rows.saturating_mul(self.width);
        let c1 = c0.saturating_add(max_rows.saturating_mul(self.width));
        let spans = self.styled_spans(idx, c0, c1);
        if spans.is_empty() {
            return Vec::new();
        }
        wrap_line(Line::from(spans), self.width)
    }

    /// The styled spans for characters `[c0, c1)` of raw line `idx`. Plain
    /// content yields a single unstyled span; ANSI content splits the slice at
    /// its style-run boundaries so each run keeps its colour.
    fn styled_spans(&self, idx: usize, c0: usize, c1: usize) -> Vec<Span<'static>> {
        if c1 <= c0 {
            return Vec::new();
        }
        let text = self.line_text(idx);
        let slice: String = text.chars().skip(c0).take(c1 - c0).collect();
        if slice.is_empty() {
            return Vec::new();
        }
        let runs = match &self.line_styles {
            None => return vec![Span::raw(slice)],
            Some(all) => all.get(idx).map(|v| v.as_slice()).unwrap_or(&[]),
        };
        if runs.is_empty() {
            return vec![Span::raw(slice)];
        }
        let style_at = |abs: usize| {
            runs.iter()
                .find(|&&(s, e, _)| abs >= s && abs < e)
                .map(|&(_, _, st)| st)
                .unwrap_or_default()
        };
        let chars: Vec<char> = slice.chars().collect();
        let mut spans = Vec::new();
        let mut i = 0usize;
        while i < chars.len() {
            let style = style_at(c0 + i);
            let mut j = i + 1;
            while j < chars.len() && style_at(c0 + j) == style {
                j += 1;
            }
            let seg: String = chars[i..j].iter().collect();
            spans.push(Span::styled(seg, style));
            i = j;
        }
        spans
    }

    /// Convert a logical [`TextPos`] into its absolute wrapped-row index and
    /// column-within-that-row — the reverse of [`Self::row_col_to_textpos`],
    /// used to project a (resize-invariant) selection back onto the current
    /// frame's screen space for highlighting or scroll-into-view.
    pub fn textpos_to_row_col(&self, pos: TextPos) -> (u32, usize) {
        if self.line_count() == 0 {
            return (0, 0);
        }
        let line = pos.line.min(self.line_count() - 1);
        let len = self.line_char_len(line);
        let col = pos.col.min(len);
        // In clip mode every raw line is exactly one row, so the row is just
        // the line's cumulative index and the column maps straight through.
        if self.mode == WrapMode::Clip || self.width == 0 {
            return (self.rows.cum[line], col);
        }
        let rows_in_line = wrapped_row_count(len, self.width) as u32;
        let row_in_line = ((col / self.width) as u32).min(rows_in_line.saturating_sub(1));
        let col_in_row = col.saturating_sub(row_in_line as usize * self.width);
        (self.rows.cum[line] + row_in_line, col_in_row)
    }

    /// Convert an absolute wrapped-row index + column-in-row (screen space)
    /// into the logical [`TextPos`] it corresponds to — the reverse of
    /// [`Self::textpos_to_row_col`], used to map a mouse click/drag onto
    /// real content.
    pub fn row_col_to_textpos(&self, row: u32, col: usize) -> TextPos {
        if self.line_count() == 0 {
            return TextPos::new(0, 0);
        }
        let (line, row_in_line) = self.rows.locate(row);
        let len = self.line_char_len(line);
        let base = if self.width == 0 {
            0
        } else {
            row_in_line as usize * self.width
        };
        // `col` may be `usize::MAX` (callers use this to mean "clamp to the
        // end of the line", e.g. auto-scroll snapping the selection cursor
        // to a row's last character) — add with saturation so that intent
        // doesn't overflow before the `.min(len)` clamp gets a chance to
        // apply.
        TextPos::new(line, base.saturating_add(col).min(len))
    }
}

/// Parse ANSI-coloured `raw` into per-line plain text plus per-line style runs
/// `(char_from, char_to_exclusive, style)`. The two are produced in one pass so
/// they stay exactly aligned character-for-character.
#[cfg(feature = "ansi")]
fn parse_ansi(raw: &str) -> (Vec<String>, LineStyles) {
    use ansi_to_tui::IntoText;
    use ratatui::text::Text;

    let text = raw
        .into_text()
        .unwrap_or_else(|_| Text::raw(raw.to_string()));
    let mut plain_lines: Vec<String> = Vec::with_capacity(text.lines.len().max(1));
    let mut styles: LineStyles = Vec::with_capacity(text.lines.len().max(1));
    for line in &text.lines {
        let mut plain = String::new();
        let mut runs: Vec<(usize, usize, Style)> = Vec::new();
        let mut col = 0usize;
        for span in &line.spans {
            let content: &str = span.content.as_ref();
            let n = content.chars().count();
            if n == 0 {
                continue;
            }
            runs.push((col, col + n, line.style.patch(span.style)));
            plain.push_str(content);
            col += n;
        }
        // A trailing carriage return belongs to the line ending, not the line
        // (matching `str::lines()`); drop it and clamp the last run.
        if plain.ends_with('\r') {
            plain.pop();
            let new_len = plain.chars().count();
            if let Some(last) = runs.last_mut() {
                last.1 = last.1.min(new_len);
                if last.0 >= last.1 {
                    runs.pop();
                }
            }
        }
        plain_lines.push(plain);
        styles.push(runs);
    }
    if plain_lines.is_empty() {
        plain_lines.push(String::new());
        styles.push(Vec::new());
    }
    (plain_lines, styles)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(text: &str, width: usize) -> PanelWrap {
        PanelWrap::build(Arc::from(text), width)
    }

    fn clip(text: &str, width: usize) -> PanelWrap {
        PanelWrap::build_with(Arc::from(text), width, WrapMode::Clip)
    }

    fn row_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn clip_mode_maps_one_row_per_line_regardless_of_length() {
        // Two lines, each far wider than the width; clip keeps them 1 row each.
        let w = clip("0123456789ABCDE\nshort", 10);
        assert_eq!(w.line_count(), 2);
        assert_eq!(w.total_rows(), 2, "one row per raw line, no wrapping");
        // Row 0 is the (clipped) first 10 chars of the long line; row 1 is the
        // whole short line.
        let rows = w.visible_window(0, 5);
        assert_eq!(rows.len(), 2);
        assert_eq!(row_text(&rows[0]), "0123456789", "clipped to width");
        assert_eq!(row_text(&rows[1]), "short");
    }

    #[test]
    fn clip_mode_row_and_textpos_map_straight_through() {
        let w = clip("0123456789ABCDE\nsecond", 10);
        // Wrapped row == line index; column maps 1:1 (no wrap offset).
        assert_eq!(w.textpos_to_row_col(TextPos::new(1, 3)), (1, 3));
        assert_eq!(w.row_col_to_textpos(1, 3), TextPos::new(1, 3));
        // A column past the clip width still resolves to the same line.
        assert_eq!(w.row_col_to_textpos(0, 4), TextPos::new(0, 4));
    }

    #[test]
    fn clip_mode_scrolls_by_whole_lines() {
        let body: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        let w = clip(&body, 4); // width 4 clips "line N" to "line"
        let rows = w.visible_window(500, 3);
        assert_eq!(rows.len(), 3);
        assert_eq!(row_text(&rows[0]), "line");
        // Each visible line is clipped to 4 chars but still one row per line.
        assert_eq!(w.total_rows(), 1000);
    }

    #[test]
    fn splits_lines_like_str_lines_including_trailing_newline_and_crlf() {
        let w = wrap("a\r\nb\nc", 10);
        assert_eq!(w.line_count(), 3);
        assert_eq!(w.line_text(0), "a");
        assert_eq!(w.line_text(1), "b");
        assert_eq!(w.line_text(2), "c");

        let w2 = wrap("a\nb\n", 10);
        assert_eq!(
            w2.line_count(),
            2,
            "no trailing empty line after a final \\n, matching str::lines()"
        );
    }

    #[test]
    fn empty_body_has_one_line_and_one_row() {
        let w = wrap("", 10);
        assert_eq!(w.line_count(), 1);
        assert_eq!(w.total_rows(), 1);
    }

    #[test]
    fn total_rows_accounts_for_wrapping_long_lines() {
        // "0123456789ABCDE" (15 chars) at width 10 -> 2 rows; "" -> 1 row.
        let w = wrap("0123456789ABCDE\n", 10);
        assert_eq!(w.total_rows(), 2);
    }

    #[test]
    fn row_col_and_textpos_roundtrip_for_a_wrapped_line() {
        let w = wrap("0123456789ABCDE", 10); // rows 0: "0123456789", row 1: "ABCDE"
        assert_eq!(w.row_col_to_textpos(0, 3), TextPos::new(0, 3));
        assert_eq!(w.row_col_to_textpos(1, 2), TextPos::new(0, 12));
        assert_eq!(w.textpos_to_row_col(TextPos::new(0, 3)), (0, 3));
        assert_eq!(w.textpos_to_row_col(TextPos::new(0, 12)), (1, 2));
        // A position exactly at the line's own length (cursor "past the end").
        assert_eq!(w.textpos_to_row_col(TextPos::new(0, 15)), (1, 5));
    }

    #[test]
    fn locate_binary_search_finds_the_right_line_for_a_huge_body() {
        let body: String = (0..100_000).map(|i| format!("line {i}\n")).collect();
        let w = wrap(&body, 20);
        // "line 50000" is 10 chars; at width 20 that's 1 row per line, so
        // wrapped-row 50_000 should land exactly on line 50_000, col 0.
        assert_eq!(w.row_col_to_textpos(50_000, 0), TextPos::new(50_000, 0));
    }

    #[test]
    fn visible_window_only_wraps_the_requested_rows() {
        let body: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        let w = wrap(&body, 20);
        let rows = w.visible_window(500, 5);
        assert_eq!(rows.len(), 5);
        let text: Vec<String> = rows
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(
            text,
            vec!["line 500", "line 501", "line 502", "line 503", "line 504"]
        );
    }

    /// A single raw line with no newlines at all (e.g. a huge base64 blob or
    /// minified JSON payload) must still produce a correct, small window
    /// regardless of where the scroll offset falls inside it — and must do
    /// so without ever wrapping the whole line (this used to cost O(line
    /// length) per redraw and grind the app to a halt; see also the timing
    /// regression test below).
    #[test]
    fn visible_window_is_correct_for_a_single_enormous_unbroken_line() {
        let body: String = "abcdefghij".repeat(200_000); // 2,000,000 chars, one line
        let w = wrap(&body, 10);

        let top = w.visible_window(0, 3);
        assert_eq!(top.len(), 3);
        let row0: String = top[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(row0, "abcdefghij", "row 0 is chars [0, 10)");
        let row2: String = top[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(
            row2, "abcdefghij",
            "row 2 (chars [20, 30)) lands mid-repeat but still aligned"
        );

        // Deep into the line: row 50_000 covers chars [500_000, 500_010).
        let mid = w.visible_window(50_000, 2);
        assert_eq!(mid.len(), 2);
        let mid_row: String = mid[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(mid_row, "abcdefghij");

        // Repeated calls with the same (scroll, height) hit the cache and
        // must return identical content.
        let again = w.visible_window(50_000, 2);
        let again_text: Vec<String> = again
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let mid_text: Vec<String> = mid
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(again_text, mid_text);
    }

    /// Regression test for the reported "obscenely large response makes the
    /// whole app grind to a halt" bug: a single multi-megabyte unwrapped
    /// line used to cost O(line length) on *every single redraw* (both in
    /// `visible_window`'s per-line `wrap_line` call and in
    /// `PanelWrap::line_char_len`'s repeated `.chars().count()`), which
    /// alone took >100ms per frame for a 5MB line. This asserts many
    /// repeated redraws of such a line stay fast, with a bound generous
    /// enough not to flake on slow CI hardware while still catching an
    /// accidental return to O(line length)-per-frame behaviour.
    #[test]
    fn visible_window_stays_fast_across_many_redraws_of_a_single_huge_line() {
        use std::time::{Duration, Instant};
        let body: String = "x".repeat(5_000_000);
        let w = wrap(&body, 78);

        let start = Instant::now();
        for _ in 0..200 {
            let rows = w.visible_window(0, 30);
            assert_eq!(
                rows.len(),
                30,
                "the first 30 wrapped rows of a 5,000,000-char line at width 78"
            );
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "200 redraws of a single 5MB line took {elapsed:?} — expected a small fraction of a second"
        );
    }

    #[test]
    fn rebuild_if_needed_skips_rebuilding_on_an_unchanged_pointer_and_width() {
        let source: Arc<str> = Arc::from("hello\nworld");
        let mut cache: Option<PanelWrap> = None;
        PanelWrap::rebuild_if_needed(&mut cache, &source, 10);
        let first_ptr = cache.as_ref().unwrap().source.as_ptr();
        // Same Arc, same width -> must not rebuild (same backing pointer).
        PanelWrap::rebuild_if_needed(&mut cache, &source, 10);
        assert_eq!(cache.as_ref().unwrap().source.as_ptr(), first_ptr);
        // Width changed -> must rebuild.
        PanelWrap::rebuild_if_needed(&mut cache, &source, 20);
        assert_eq!(cache.as_ref().unwrap().width, 20);
        // A genuinely new Arc (even with equal content) -> must rebuild too,
        // since a new response/edit always allocates fresh.
        let source2: Arc<str> = Arc::from("hello\nworld");
        PanelWrap::rebuild_if_needed(&mut cache, &source2, 20);
        assert!(Arc::ptr_eq(&cache.as_ref().unwrap().source, &source2));
    }
}

#[cfg(all(test, feature = "ansi"))]
mod ansi_tests {
    use super::*;
    use ratatui::style::Color;

    fn row_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    const RED_THEN_PLAIN: &str = "\x1b[31mred\x1b[0m plain";

    #[test]
    fn geometry_and_copy_use_the_stripped_text() {
        let w = PanelWrap::build_ansi(Arc::from(RED_THEN_PLAIN), 40, WrapMode::Wrap);
        // Selection/geometry see the plain text, not the escape sequences.
        assert_eq!(w.line_count(), 1);
        assert_eq!(w.line_text(0), "red plain");
        assert_eq!(w.line_char_len(0), 9);
    }

    #[test]
    fn rendered_rows_keep_their_colour() {
        let w = PanelWrap::build_ansi(Arc::from(RED_THEN_PLAIN), 40, WrapMode::Wrap);
        let rows = w.visible_window(0, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(row_text(&rows[0]), "red plain");
        // First span is the red "red"; the rest is unstyled " plain".
        assert_eq!(rows[0].spans[0].content.as_ref(), "red");
        assert_eq!(rows[0].spans[0].style.fg, Some(Color::Red));
        let plain: String = rows[0].spans[1..]
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(plain, " plain");
        assert_ne!(
            rows[0].spans[1].style.fg,
            Some(Color::Red),
            "the reset run is not red"
        );
    }

    #[test]
    fn colour_survives_wrapping_across_a_row_boundary() {
        // "red" (3) + " plain" (6) = 9 chars; width 4 wraps to 3 rows.
        let w = PanelWrap::build_ansi(Arc::from(RED_THEN_PLAIN), 4, WrapMode::Wrap);
        assert_eq!(w.total_rows(), 3);
        let rows = w.visible_window(0, 3);
        assert_eq!(row_text(&rows[0]), "red ");
        // The 'd' at the wrap boundary keeps the red colour.
        assert_eq!(rows[0].spans[0].content.as_ref(), "red");
        assert_eq!(rows[0].spans[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn clip_mode_keeps_colour_on_the_single_clipped_row() {
        let w = PanelWrap::build_ansi(Arc::from(RED_THEN_PLAIN), 4, WrapMode::Clip);
        assert_eq!(w.total_rows(), 1);
        let rows = w.visible_window(0, 5);
        assert_eq!(rows.len(), 1);
        assert_eq!(row_text(&rows[0]), "red ", "clipped to width 4");
        assert_eq!(rows[0].spans[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn ansi_and_plain_switch_forces_a_rebuild() {
        let raw: Arc<str> = Arc::from(RED_THEN_PLAIN);
        let mut cache: Option<PanelWrap> = None;
        PanelWrap::rebuild_if_needed_ansi(&mut cache, &raw, 40, WrapMode::Wrap);
        assert!(cache.as_ref().unwrap().line_styles.is_some());
        // Same Arc + width + mode -> no rebuild.
        let ptr = cache.as_ref().unwrap().source.as_ptr();
        PanelWrap::rebuild_if_needed_ansi(&mut cache, &raw, 40, WrapMode::Wrap);
        assert_eq!(cache.as_ref().unwrap().source.as_ptr(), ptr);
        // Switching to the plain builder must rebuild (styled -> unstyled).
        PanelWrap::rebuild_if_needed_with(&mut cache, &raw, 40, WrapMode::Wrap);
        assert!(cache.as_ref().unwrap().line_styles.is_none());
    }
}
