//! The `DocumentFormatter` forms the bridge between the raw document text
//! and onscreen positioning. It yields the text graphemes as an iterator
//! and traverses (part) of the document text. During that traversal it
//! handles grapheme detection, softwrapping and annotations.
//! It yields `FormattedGrapheme`s and their corresponding visual coordinates.
//!
//! As both virtual text and softwrapping can insert additional lines into the document
//! it is generally not possible to find the start of the previous visual line.
//! Instead the `DocumentFormatter` starts at the last "checkpoint" (usually a linebreak)
//! called a "block" and the caller must advance it as needed.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::fmt::Debug;
use std::mem::replace;

#[cfg(test)]
mod test;

use unicode_segmentation::{Graphemes, UnicodeSegmentation};

use helix_stdx::rope::{RopeGraphemes, RopeSliceExt};

use crate::chars::char_is_line_ending;
use crate::graphemes::{Grapheme, GraphemeStr};
use crate::syntax::Highlight;
use crate::text_annotations::TextAnnotations;
use crate::{Position, RopeSlice};

#[derive(Debug, Clone, Copy)]
pub enum GraphemeSource {
    Document {
        codepoints: u32,
    },
    /// Inline virtual text can not be highlighted with a `Highlight` iterator
    /// because it's not part of the document. Instead the `Highlight`
    /// is emitted right by the document formatter
    VirtualText {
        highlight: Option<Highlight>,
    },
    /// A fold marker. It stands in for a hidden range: it draws like virtual
    /// text but bumps the document char position by `codepoints` and the line
    /// position by `lines`, so cursor mapping, scrolling and line numbers all
    /// skip the hidden lines for free.
    Fold {
        highlight: Option<Highlight>,
        codepoints: u32,
        lines: u32,
    },
}

impl GraphemeSource {
    /// Returns whether this grapheme is virtual inline text
    pub fn is_virtual(self) -> bool {
        matches!(
            self,
            GraphemeSource::VirtualText { .. } | GraphemeSource::Fold { .. }
        )
    }

    pub fn is_eof(self) -> bool {
        // all doc chars except the EOF char have non-zero codepoints
        matches!(self, GraphemeSource::Document { codepoints: 0 })
    }

    pub fn doc_chars(self) -> usize {
        match self {
            GraphemeSource::Document { codepoints } => codepoints as usize,
            // a fold marker stands in for the whole hidden range, so the char
            // position jumps past it
            GraphemeSource::Fold { codepoints, .. } => codepoints as usize,
            GraphemeSource::VirtualText { .. } => 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FormattedGrapheme<'a> {
    pub raw: Grapheme<'a>,
    pub source: GraphemeSource,
    pub visual_pos: Position,
    /// Document line at the start of the grapheme
    pub line_idx: usize,
    /// Document char position at the start of the grapheme
    pub char_idx: usize,
}

impl FormattedGrapheme<'_> {
    pub fn is_virtual(&self) -> bool {
        self.source.is_virtual()
    }

    pub fn doc_chars(&self) -> usize {
        self.source.doc_chars()
    }

    pub fn is_whitespace(&self) -> bool {
        self.raw.is_whitespace()
    }

    pub fn width(&self) -> usize {
        self.raw.width()
    }

    pub fn is_word_boundary(&self) -> bool {
        self.raw.is_word_boundary()
    }
}

#[derive(Debug, Clone)]
struct GraphemeWithSource<'a> {
    grapheme: Grapheme<'a>,
    source: GraphemeSource,
    /// Marks a line end grapheme and the annotations rendered after it
    after_line_end: bool,
}

impl<'a> GraphemeWithSource<'a> {
    fn new(
        g: GraphemeStr<'a>,
        visual_x: usize,
        tab_width: u16,
        source: GraphemeSource,
    ) -> GraphemeWithSource<'a> {
        GraphemeWithSource {
            grapheme: Grapheme::new(g, visual_x, tab_width),
            source,
            after_line_end: false,
        }
    }
    fn placeholder() -> Self {
        GraphemeWithSource {
            grapheme: Grapheme::Other { g: " ".into() },
            source: GraphemeSource::Document { codepoints: 0 },
            after_line_end: false,
        }
    }

    fn doc_chars(&self) -> usize {
        self.source.doc_chars()
    }

    fn is_whitespace(&self) -> bool {
        self.grapheme.is_whitespace()
    }

    fn is_newline(&self) -> bool {
        matches!(self.grapheme, Grapheme::Newline)
    }

    fn is_eof(&self) -> bool {
        self.source.is_eof()
    }

    fn width(&self) -> usize {
        self.grapheme.width()
    }

    fn is_word_boundary(&self) -> bool {
        self.grapheme.is_word_boundary()
    }
}

#[derive(Debug, Clone)]
pub struct TextFormat {
    pub soft_wrap: bool,
    pub tab_width: u16,
    pub max_wrap: u16,
    pub max_indent_retain: u16,
    pub wrap_indicator: Box<str>,
    pub wrap_indicator_highlight: Option<Highlight>,
    pub viewport_width: u16,
    pub soft_wrap_at_text_width: bool,
    /// The single grapheme drawn on a fold header where the hidden lines were.
    pub fold_marker: Box<str>,
    pub fold_marker_highlight: Option<Highlight>,
}

// test implementation is basically only used for testing or when softwrap is always disabled
impl Default for TextFormat {
    fn default() -> Self {
        TextFormat {
            soft_wrap: false,
            tab_width: 4,
            max_wrap: 3,
            max_indent_retain: 4,
            wrap_indicator: Box::from(" "),
            viewport_width: 17,
            wrap_indicator_highlight: None,
            soft_wrap_at_text_width: false,
            fold_marker: Box::from("⋯"),
            fold_marker_highlight: None,
        }
    }
}

/// A fold marker emitted one cell at a time; the hidden char/line counts are
/// attached to its final cell
#[derive(Debug)]
struct FoldMarkerGraphemes<'t> {
    graphemes: Graphemes<'t>,
    highlight: Option<Highlight>,
    codepoints: u32,
    lines: u32,
}

#[derive(Debug)]
pub struct DocumentFormatter<'t> {
    text_fmt: &'t TextFormat,
    annotations: &'t TextAnnotations<'t>,

    /// The visual position at the end of the last yielded word boundary
    visual_pos: Position,
    graphemes: RopeGraphemes<'t>,
    /// The character pos of the `graphemes` iter used for inserting annotations
    char_pos: usize,
    /// The line pos of the `graphemes` iter used for inserting annotations
    line_pos: usize,
    exhausted: bool,

    inline_annotation_graphemes: Option<(Graphemes<'t>, Option<Highlight>)>,

    /// The full document text, used to look ahead at line ends
    text: RopeSlice<'t>,
    /// Anchor of line end annotations still being emitted
    eol_annotation_anchor: Option<usize>,
    /// Row advance deferred until the line end annotations are emitted
    pending_eol_row_advance: bool,

    /// A fold marker emitted one cell at a time (see next_fold_marker_grapheme)
    fold_marker_graphemes: Option<FoldMarkerGraphemes<'t>>,

    // softwrap specific
    /// The indentation of the current line
    /// Is set to `None` if the indentation level is not yet known
    /// because no non-whitespace graphemes have been encountered yet
    indent_level: Option<usize>,
    /// In case a long word needs to be split a single grapheme might need to be wrapped
    /// while the rest of the word stays on the same line
    peeked_grapheme: Option<GraphemeWithSource<'t>>,
    /// A first-in first-out (fifo) buffer for the Graphemes of any given word
    word_buf: Vec<GraphemeWithSource<'t>>,
    /// The index of the next grapheme that will be yielded from the `word_buf`
    word_i: usize,
}

impl<'t> DocumentFormatter<'t> {
    /// Creates a new formatter at the last block before `char_idx`.
    /// A block is a chunk which always ends with a linebreak.
    /// This is usually just a normal line break.
    /// However very long lines are always wrapped at constant intervals that can be cheaply calculated
    /// to avoid pathological behaviour.
    pub fn new_at_prev_checkpoint(
        text: RopeSlice<'t>,
        text_fmt: &'t TextFormat,
        annotations: &'t TextAnnotations,
        char_idx: usize,
    ) -> Self {
        // TODO divide long lines into blocks to avoid bad performance for long lines
        let block_line_idx = text.char_to_line(char_idx.min(text.len_chars()));
        let mut block_char_idx = text.line_to_char(block_line_idx);
        // don't start drawing from inside a fold. jump back to the fold's header
        // line (where the marker is), not forward past it. going backward keeps
        // the walk in char_idx_at_visual_offset shrinking toward 0; going forward
        // makes it grow instead and the loop spins forever (that was the freeze).
        if let Some(fold) = annotations.fold_containing(block_char_idx) {
            let header_line = text.char_to_line(fold.start_char.min(text.len_chars()));
            block_char_idx = text.line_to_char(header_line);
        }
        let block_line_idx = text.char_to_line(block_char_idx);
        annotations.reset_pos(block_char_idx);

        DocumentFormatter {
            text_fmt,
            annotations,
            visual_pos: Position { row: 0, col: 0 },
            graphemes: text.slice(block_char_idx..).graphemes(),
            char_pos: block_char_idx,
            exhausted: false,
            indent_level: None,
            peeked_grapheme: None,
            word_buf: Vec::with_capacity(64),
            word_i: 0,
            line_pos: block_line_idx,
            inline_annotation_graphemes: None,
            text,
            eol_annotation_anchor: None,
            pending_eol_row_advance: false,
            fold_marker_graphemes: None,
        }
    }

    fn next_inline_annotation_grapheme(
        &mut self,
        char_pos: usize,
    ) -> Option<(&'t str, Option<Highlight>)> {
        loop {
            if let Some(&mut (ref mut annotation, highlight)) =
                self.inline_annotation_graphemes.as_mut()
            {
                if let Some(grapheme) = annotation.next() {
                    return Some((grapheme, highlight));
                }
            }

            if let Some((annotation, highlight)) =
                self.annotations.next_inline_annotation_at(char_pos)
            {
                self.inline_annotation_graphemes = Some((
                    UnicodeSegmentation::graphemes(&*annotation.text, true),
                    highlight,
                ))
            } else {
                return None;
            }
        }
    }

    /// Yield the next cell of a fold marker mid-emission. Each grapheme is its
    /// own cell so a multi-char marker like `" ... "` draws correctly; the last
    /// cell carries the hidden char/line counts, the rest are virtual text
    fn next_fold_marker_grapheme(&mut self, col: usize) -> Option<GraphemeWithSource<'t>> {
        let marker = self.fold_marker_graphemes.as_mut()?;
        let grapheme = marker.graphemes.next()?;
        let is_last = marker.graphemes.clone().next().is_none();
        let source = if is_last {
            GraphemeSource::Fold {
                highlight: marker.highlight,
                codepoints: marker.codepoints,
                lines: marker.lines,
            }
        } else {
            GraphemeSource::VirtualText {
                highlight: marker.highlight,
            }
        };
        let grapheme =
            GraphemeWithSource::new(grapheme.into(), col, self.text_fmt.tab_width, source);
        if is_last {
            self.fold_marker_graphemes = None;
        }
        Some(grapheme)
    }

    fn advance_grapheme(&mut self, col: usize, char_pos: usize) -> Option<GraphemeWithSource<'t>> {
        // drain the annotations of an already emitted line end
        if let Some(anchor) = self.eol_annotation_anchor {
            if let Some((grapheme, highlight)) = self.next_inline_annotation_grapheme(anchor) {
                let mut grapheme = GraphemeWithSource::new(
                    grapheme.into(),
                    col,
                    self.text_fmt.tab_width,
                    GraphemeSource::VirtualText { highlight },
                );
                grapheme.after_line_end = true;
                return Some(grapheme);
            }
            self.eol_annotation_anchor = None;
        }

        // keep draining a marker started on a previous call
        if let Some(grapheme) = self.next_fold_marker_grapheme(col) {
            return Some(grapheme);
        }

        // a fold starts here: skip the hidden graphemes, then emit its marker
        if let Some(fold) = self.annotations.fold_starting_at(char_pos) {
            let span = fold.end_char.saturating_sub(char_pos);
            let mut skipped = 0;
            let mut lines = 0u32;
            while skipped < span {
                match self.graphemes.next() {
                    Some(grapheme) => {
                        skipped += grapheme.len_chars();
                        lines += grapheme.chars().filter(|&c| c == '\n').count() as u32;
                    }
                    None => break,
                }
            }
            self.fold_marker_graphemes = Some(FoldMarkerGraphemes {
                graphemes: UnicodeSegmentation::graphemes(self.text_fmt.fold_marker.as_ref(), true),
                highlight: self.text_fmt.fold_marker_highlight,
                codepoints: skipped as u32,
                lines,
            });
            return self.next_fold_marker_grapheme(col);
        }

        // annotations anchored at a line end render after it so a cursor
        // at the end of the actual text stays in front of the virtual text
        let line_end_annotations =
            self.at_line_end(char_pos) && self.annotations.has_inline_annotation_at(char_pos);

        let annotation = if line_end_annotations {
            self.eol_annotation_anchor = Some(char_pos);
            None
        } else {
            self.next_inline_annotation_grapheme(char_pos)
        };

        let (grapheme, source) = if let Some((grapheme, highlight)) = annotation {
            (grapheme.into(), GraphemeSource::VirtualText { highlight })
        } else if let Some(grapheme) = self.graphemes.next() {
            let codepoints = grapheme.len_chars() as u32;

            let overlay = self.annotations.overlay_at(char_pos);
            let grapheme = match overlay {
                Some((overlay, _)) => overlay.grapheme.as_str().into(),
                None => Cow::from(grapheme).into(),
            };

            (grapheme, GraphemeSource::Document { codepoints })
        } else {
            if self.exhausted {
                return None;
            }
            self.exhausted = true;
            // EOF grapheme is required for rendering
            // and correct position computations
            return Some(GraphemeWithSource {
                grapheme: Grapheme::Other { g: " ".into() },
                source: GraphemeSource::Document { codepoints: 0 },
                after_line_end: line_end_annotations,
            });
        };

        let mut grapheme = GraphemeWithSource::new(grapheme, col, self.text_fmt.tab_width, source);
        grapheme.after_line_end = line_end_annotations;

        Some(grapheme)
    }

    /// Whether the grapheme at `char_pos` is a line break or the end of the text
    fn at_line_end(&self, char_pos: usize) -> bool {
        char_pos >= self.text.len_chars() || char_is_line_ending(self.text.char(char_pos))
    }

    /// Move a word to the next visual line
    fn wrap_word(&mut self) -> usize {
        // softwrap this word to the next line
        let indent_carry_over = if let Some(indent) = self.indent_level {
            if indent as u16 <= self.text_fmt.max_indent_retain {
                indent as u16
            } else {
                0
            }
        } else {
            // ensure the indent stays 0
            self.indent_level = Some(0);
            0
        };

        let virtual_lines =
            self.annotations
                .virtual_lines_at(self.char_pos, self.visual_pos, self.line_pos);
        self.visual_pos.col = indent_carry_over as usize;
        self.visual_pos.row += 1 + virtual_lines;
        let mut i = 0;
        let mut word_width = 0;
        let wrap_indicator = UnicodeSegmentation::graphemes(&*self.text_fmt.wrap_indicator, true)
            .map(|g| {
                i += 1;
                let grapheme = GraphemeWithSource::new(
                    g.into(),
                    self.visual_pos.col + word_width,
                    self.text_fmt.tab_width,
                    GraphemeSource::VirtualText {
                        highlight: self.text_fmt.wrap_indicator_highlight,
                    },
                );
                word_width += grapheme.width();
                grapheme
            });
        self.word_buf.splice(0..0, wrap_indicator);

        for grapheme in &mut self.word_buf[i..] {
            let visual_x = self.visual_pos.col + word_width;
            grapheme
                .grapheme
                .change_position(visual_x, self.text_fmt.tab_width);
            word_width += grapheme.width();
        }
        if let Some(grapheme) = &mut self.peeked_grapheme {
            let visual_x = self.visual_pos.col + word_width;
            grapheme
                .grapheme
                .change_position(visual_x, self.text_fmt.tab_width);
        }
        word_width
    }

    fn peek_grapheme(&mut self, col: usize, char_pos: usize) -> Option<&GraphemeWithSource<'t>> {
        if self.peeked_grapheme.is_none() {
            self.peeked_grapheme = self.advance_grapheme(col, char_pos);
        }
        self.peeked_grapheme.as_ref()
    }

    fn next_grapheme(&mut self, col: usize, char_pos: usize) -> Option<GraphemeWithSource<'t>> {
        self.peek_grapheme(col, char_pos);
        self.peeked_grapheme.take()
    }

    fn advance_to_next_word(&mut self) {
        self.word_buf.clear();
        let mut word_width = 0;
        let mut word_chars = 0;

        if self.exhausted {
            return;
        }

        loop {
            let mut col = self.visual_pos.col + word_width;
            let char_pos = self.char_pos + word_chars;
            match col.cmp(&(self.text_fmt.viewport_width as usize)) {
                // The EOF char and newline chars are always selectable in helix. That means
                // that wrapping happens "too-early" if a word fits a line perfectly. This
                // is intentional so that all selectable graphemes are always visible (and
                // therefore the cursor never disappears). However if the user manually set a
                // lower softwrap width then this is undesirable. Just increasing the viewport-
                // width by one doesn't work because if a line is wrapped multiple times then
                // some words may extend past the specified width.
                //
                // So we special case a word that ends exactly at line bounds and is followed
                // by a newline/eof character here.
                Ordering::Equal
                    if self.text_fmt.soft_wrap_at_text_width
                        && self
                            .peek_grapheme(col, char_pos)
                            .is_some_and(|grapheme| grapheme.is_newline() || grapheme.is_eof()) => {
                }
                Ordering::Equal if word_width > self.text_fmt.max_wrap as usize => return,
                Ordering::Greater if word_width > self.text_fmt.max_wrap as usize => {
                    self.peeked_grapheme = self.word_buf.pop();
                    return;
                }
                Ordering::Equal | Ordering::Greater => {
                    word_width = self.wrap_word();
                    col = self.visual_pos.col + word_width;
                }
                Ordering::Less => (),
            }

            let Some(grapheme) = self.next_grapheme(col, char_pos) else {
                return;
            };
            word_chars += grapheme.doc_chars();

            // Track indentation
            if !grapheme.is_whitespace() && self.indent_level.is_none() {
                self.indent_level = Some(self.visual_pos.col);
            } else if grapheme.grapheme == Grapheme::Newline {
                self.indent_level = None;
            }

            let is_word_boundary = grapheme.is_word_boundary();
            word_width += grapheme.width();
            self.word_buf.push(grapheme);

            if is_word_boundary {
                return;
            }
        }
    }

    /// returns the char index at the end of the last yielded grapheme
    pub fn next_char_pos(&self) -> usize {
        self.char_pos
    }
    /// returns the visual position at the end of the last yielded grapheme
    pub fn next_visual_pos(&self) -> Position {
        self.visual_pos
    }
}

impl<'t> Iterator for DocumentFormatter<'t> {
    type Item = FormattedGrapheme<'t>;

    fn next(&mut self) -> Option<Self::Item> {
        let grapheme = if self.text_fmt.soft_wrap {
            if self.word_i >= self.word_buf.len() {
                self.advance_to_next_word();
                self.word_i = 0;
            }
            let grapheme = replace(
                self.word_buf.get_mut(self.word_i)?,
                GraphemeWithSource::placeholder(),
            );
            self.word_i += 1;
            grapheme
        } else {
            self.advance_grapheme(self.visual_pos.col, self.char_pos)?
        };
        let after_line_end = grapheme.after_line_end;

        // perform the row advance deferred by the previous line end
        if self.pending_eol_row_advance && !after_line_end {
            let virtual_lines =
                self.annotations
                    .virtual_lines_at(self.char_pos, self.visual_pos, self.line_pos);
            self.visual_pos.row += 1 + virtual_lines;
            self.visual_pos.col = 0;
            self.line_pos += 1;
            self.pending_eol_row_advance = false;
        }

        let grapheme = FormattedGrapheme {
            raw: grapheme.grapheme,
            source: grapheme.source,
            visual_pos: self.visual_pos,
            line_idx: self.line_pos,
            char_idx: self.char_pos,
        };

        self.char_pos += grapheme.doc_chars();
        // a fold marker hides whole lines; bump the document line position past
        // them so line numbers and anchors stay correct after the fold
        if let GraphemeSource::Fold { lines, .. } = grapheme.source {
            self.line_pos += lines as usize;
        }
        if !grapheme.is_virtual() {
            self.annotations.process_virtual_text_anchors(&grapheme);
        }
        if grapheme.raw == Grapheme::Newline {
            if after_line_end {
                // annotations still follow on this row, defer the row advance
                self.visual_pos.col += 1;
                self.pending_eol_row_advance = true;
            } else {
                // move to end of newline char
                self.visual_pos.col += 1;
                let virtual_lines =
                    self.annotations
                        .virtual_lines_at(self.char_pos, self.visual_pos, self.line_pos);
                self.visual_pos.row += 1 + virtual_lines;
                self.visual_pos.col = 0;
                if !grapheme.is_virtual() {
                    self.line_pos += 1;
                }
            }
        } else {
            self.visual_pos.col += grapheme.width();
        }
        Some(grapheme)
    }
}
