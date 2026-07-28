//! Folds hide lines when drawing without changing the actual text, so the LSP
//! still sees the whole file.

use crate::RopeSlice;

/// A hidden range of chars. The first line and the last line still show, only
/// the stuff in between gets hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fold {
    pub start_char: usize,
    pub end_char: usize,
}

impl Fold {
    pub fn new(start_char: usize, end_char: usize) -> Self {
        Fold { start_char, end_char }
    }

    /// Is this char inside the hidden part?
    pub fn contains(&self, char_idx: usize) -> bool {
        self.start_char <= char_idx && char_idx < self.end_char
    }

    pub fn is_empty(&self) -> bool {
        self.start_char >= self.end_char
    }
}

/// All the folds for one document. Sorted by start, and they never overlap.
#[derive(Debug, Clone, Default)]
pub struct Folds {
    folds: Vec<Fold>,
}

impl Folds {
    pub fn is_empty(&self) -> bool {
        self.folds.is_empty()
    }

    pub fn len(&self) -> usize {
        self.folds.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Fold> {
        self.folds.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Fold> {
        self.folds.iter_mut()
    }

    pub fn as_slice(&self) -> &[Fold] {
        &self.folds
    }

    /// Find the fold that hides this char, if there is one.
    pub fn fold_at(&self, char_idx: usize) -> Option<&Fold> {
        self.folds.iter().find(|f| f.contains(char_idx))
    }

    /// Add a fold. If it overlaps an existing one, throw the old one out.
    pub fn insert(&mut self, fold: Fold) {
        if fold.is_empty() {
            return;
        }
        self.folds
            .retain(|f| f.end_char <= fold.start_char || f.start_char >= fold.end_char);
        let idx = self.folds.partition_point(|f| f.start_char < fold.start_char);
        self.folds.insert(idx, fold);
    }

    /// Remove the fold sitting on this char. Tells you if it actually removed one.
    pub fn remove_at(&mut self, char_idx: usize) -> bool {
        if let Some(i) = self.folds.iter().position(|f| f.contains(char_idx)) {
            self.folds.remove(i);
            true
        } else {
            false
        }
    }

    pub fn clear(&mut self) {
        self.folds.clear();
    }

    /// After an edit moves things around, drop the folds that got squished to
    /// nothing and sort again.
    pub fn prune(&mut self, text_len: usize) {
        self.folds.retain(|f| !f.is_empty() && f.start_char < text_len);
        self.folds.sort_by_key(|f| f.start_char);
    }
}

/// Char index of the newline at the end of a line. Clamps to the end of the file
/// for the very last line.
pub fn line_end_char(text: RopeSlice, line: usize) -> usize {
    let lines = text.len_lines();
    let line_start = text.line_to_char(line.min(lines));
    let next_start = text.line_to_char((line + 1).min(lines));
    next_start.saturating_sub(1).max(line_start)
}
