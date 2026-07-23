//! Open-document store and byte-offset ↔ LSP position conversion.
//!
//! The server negotiates full-text sync (`.partiri.jsonc` files are tiny), so a
//! document is just its latest text. LSP positions are UTF-16 code units per the
//! protocol default; `jsonc-parser` ranges are byte offsets — [`LineIndex`]
//! converts between the two.

use lsp_types::{Diagnostic, Position, Range};

/// Latest state of one open document.
#[derive(Default)]
pub(crate) struct DocState {
    pub(crate) text: String,
    /// Diagnostics from the local pass (parse + `validate_config`).
    pub(crate) local_diags: Vec<Diagnostic>,
    /// Diagnostics from the last remote validation run. Dropped on every edit —
    /// the config changed, so stale remote findings would mislead.
    pub(crate) remote_diags: Vec<Diagnostic>,
}

impl DocState {
    pub(crate) fn merged_diags(&self) -> Vec<Diagnostic> {
        let mut all = self.local_diags.clone();
        all.extend(self.remote_diags.clone());
        all
    }
}

/// Byte offsets of every line start, for O(log n) offset → line lookups.
pub(crate) struct LineIndex {
    line_starts: Vec<usize>,
}

impl LineIndex {
    pub(crate) fn new(text: &str) -> Self {
        let mut line_starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        Self { line_starts }
    }

    /// Convert a byte offset into an LSP [`Position`] (UTF-16 column).
    /// Offsets past the end of `text` clamp to the final position.
    pub(crate) fn position(&self, text: &str, offset: usize) -> Position {
        let offset = offset.min(text.len());
        let line = match self.line_starts.binary_search(&offset) {
            Ok(l) => l,
            Err(l) => l - 1,
        };
        let line_start = self.line_starts[line];
        let col_utf16: usize = text[line_start..offset].chars().map(char::len_utf16).sum();
        Position::new(line as u32, col_utf16 as u32)
    }

    /// Convert an LSP [`Position`] into a byte offset. Positions past the end
    /// of a line clamp to the line end; lines past the end clamp to EOF.
    pub(crate) fn offset(&self, text: &str, pos: Position) -> usize {
        let line = pos.line as usize;
        if line >= self.line_starts.len() {
            return text.len();
        }
        let line_start = self.line_starts[line];
        let line_end = self
            .line_starts
            .get(line + 1)
            .map(|s| s.saturating_sub(1))
            .unwrap_or(text.len());
        let mut utf16_remaining = pos.character as usize;
        let mut offset = line_start;
        for c in text[line_start..line_end].chars() {
            if utf16_remaining < c.len_utf16() {
                break;
            }
            utf16_remaining -= c.len_utf16();
            offset += c.len_utf8();
        }
        offset
    }

    /// Convert a byte-offset span into an LSP [`Range`].
    pub(crate) fn range(&self, text: &str, start: usize, end: usize) -> Range {
        Range::new(self.position(text, start), self.position(text, end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_ascii() {
        let text = "ab\ncd";
        let idx = LineIndex::new(text);
        assert_eq!(idx.position(text, 0), Position::new(0, 0));
        assert_eq!(idx.position(text, 2), Position::new(0, 2));
        assert_eq!(idx.position(text, 3), Position::new(1, 0));
        assert_eq!(idx.position(text, 5), Position::new(1, 2));
    }

    #[test]
    fn position_clamps_past_eof() {
        let text = "ab";
        let idx = LineIndex::new(text);
        assert_eq!(idx.position(text, 99), Position::new(0, 2));
    }

    #[test]
    fn position_utf16_for_multibyte() {
        // '€' is 3 UTF-8 bytes but 1 UTF-16 unit.
        let text = "€x";
        let idx = LineIndex::new(text);
        assert_eq!(idx.position(text, 3), Position::new(0, 1));
        assert_eq!(idx.position(text, 4), Position::new(0, 2));
    }

    #[test]
    fn offset_roundtrip() {
        let text = "{\n  \"name\": \"café\"\n}";
        let idx = LineIndex::new(text);
        for off in [0, 1, 4, 10, text.len()] {
            let pos = idx.position(text, off);
            assert_eq!(idx.offset(text, pos), off, "offset {off} must round-trip");
        }
    }

    #[test]
    fn offset_clamps_past_line_end() {
        let text = "ab\ncd";
        let idx = LineIndex::new(text);
        assert_eq!(idx.offset(text, Position::new(0, 99)), 2);
        assert_eq!(idx.offset(text, Position::new(9, 0)), 5);
    }
}
