//! Text helpers: file decoding and conversion between tree-sitter points (byte columns)
//! and LSP positions (UTF-16 columns).

use tower_lsp::lsp_types::{Position, Range};
use tree_sitter::{Node, Point};

/// Clonk files are usually Windows-1252; newer ones may be UTF-8.
pub fn decode_bytes(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| cp1252_char(b)).collect(),
    }
}

fn cp1252_char(b: u8) -> char {
    const HIGH: [u16; 32] = [
        0x20AC, 0x81, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039,
        0x0152, 0x8D, 0x017D, 0x8F, 0x90, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014,
        0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x9D, 0x017E, 0x0178,
    ];
    if (0x80..0xA0).contains(&b) {
        char::from_u32(HIGH[(b - 0x80) as usize] as u32).unwrap_or('?')
    } else {
        b as char
    }
}

/// Line start offsets of a source text, for position conversion.
pub struct LineIndex<'a> {
    src: &'a str,
    starts: Vec<usize>,
}

impl<'a> LineIndex<'a> {
    pub fn new(src: &'a str) -> Self {
        let mut starts = vec![0];
        starts.extend(src.match_indices('\n').map(|(i, _)| i + 1));
        LineIndex { src, starts }
    }

    fn line_text(&self, row: usize) -> &'a str {
        let start = *self.starts.get(row).unwrap_or(&self.src.len());
        let end = self.starts.get(row + 1).map(|e| e - 1).unwrap_or(self.src.len());
        &self.src[start.min(self.src.len())..end.max(start).min(self.src.len())]
    }

    pub fn to_lsp(&self, p: Point) -> Position {
        let line = self.line_text(p.row);
        let col = p.column.min(line.len());
        let col = (0..=col).rev().find(|&c| line.is_char_boundary(c)).unwrap_or(0);
        Position::new(p.row as u32, line[..col].encode_utf16().count() as u32)
    }

    pub fn to_point(&self, pos: Position) -> Point {
        let line = self.line_text(pos.line as usize);
        let mut units = 0u32;
        let mut col = line.len();
        for (i, ch) in line.char_indices() {
            if units >= pos.character {
                col = i;
                break;
            }
            units += ch.len_utf16() as u32;
        }
        Point { row: pos.line as usize, column: col }
    }

    pub fn node_range(&self, n: &Node) -> Range {
        Range::new(self.to_lsp(n.start_position()), self.to_lsp(n.end_position()))
    }
}

pub fn node_text<'a>(n: &Node, src: &'a str) -> &'a str {
    &src[n.start_byte().min(src.len())..n.end_byte().min(src.len())]
}
