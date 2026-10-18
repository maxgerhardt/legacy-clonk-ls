use tower_lsp::lsp_types::SemanticToken;
use tree_sitter::Node;

use super::token_types::TokenTypes;

#[derive(Debug, Default)]
pub struct Context {
    pub collection: Vec<SemanticToken>,
    pub last_line: u32,
    pub last_start: u32,
    pub token_types: TokenTypes,
    /// modifier bits for the tokens added now (bit 0 = defaultLibrary: engine functions)
    pub modifiers: u32,
}

pub fn add_semantic_token(
    ctx: &mut Context,
    token_type: u32,
    node: &Node,
) {
    let start = node.start_position();
    let mut start_row = start.row as u32;
    let start_col = start.column as u32;
    let on_same_line = ctx.last_line == start_row;

    let end = node.end_position();
    let end_row = end.row as u32;
    let end_col = end.column as u32;
    let multiline = start_row != end_row;

    if multiline {
        // TODO: If there are editors not happy with sending a token of the aribitraty length 2000
        // we need to properly pre-calculate the line lengths before

        // First line
        {
            let (delta_line, delta_start) = if on_same_line {
                (0, start_col - ctx.last_start)
            } else {
                (start_row - ctx.last_line, start_col)
            };

            ctx.collection.push(SemanticToken {
                delta_line,
                delta_start,
                length: 2000,
                token_type,
                token_modifiers_bitset: ctx.modifiers,
                ..Default::default()
            });
        }

        // Intermediate lines
        loop {
            // Make sure, we are not already on the last line
            start_row += 1;
            if start_row == end_row {
                break;
            }

            ctx.collection.push(SemanticToken {
                delta_line: 1,
                delta_start: 0,
                length: 2000,
                token_type,
                token_modifiers_bitset: ctx.modifiers,
                ..Default::default()
            });
        }

        // Last line
        ctx.collection.push(SemanticToken {
            delta_line: 1,
            delta_start: 0,
            length: end_col,
            token_type,
            token_modifiers_bitset: ctx.modifiers,
                ..Default::default()
        });

        ctx.last_line = start_row;
        ctx.last_start = 0;

    } else {
        let (delta_line, delta_start) = if on_same_line {
            (0, start_col - ctx.last_start)
        } else {
            (start_row - ctx.last_line, start_col)
        };

        if on_same_line {
            ctx.last_start += delta_start;
        } else {
            ctx.last_line = start_row;
            ctx.last_start = delta_start;
        }

        ctx.collection.push(SemanticToken {
            delta_line,
            delta_start,
            length: (node.end_position().column - node.start_position().column) as u32,
            token_type,
            token_modifiers_bitset: ctx.modifiers,
                ..Default::default()
        });
    }
}


pub fn add_semantic_token_at(
    ctx: &mut Context,
    token_type: u32,
    start: tree_sitter::Point,
    end: tree_sitter::Point,
) {
    let mut start_row = start.row as u32;
    let start_col = start.column as u32;
    let on_same_line = ctx.last_line == start_row;

    let end_row = end.row as u32;
    let end_col = end.column as u32;
    let multiline = start_row != end_row;

    if multiline {
        // TODO: If there are editors not happy with sending a token of the aribitraty length 2000
        // we need to properly pre-calculate the line lengths before

        // First line
        {
            let (delta_line, delta_start) = if on_same_line {
                (0, start_col - ctx.last_start)
            } else {
                (start_row - ctx.last_line, start_col)
            };

            ctx.collection.push(SemanticToken {
                delta_line,
                delta_start,
                length: 2000,
                token_type,
                token_modifiers_bitset: ctx.modifiers,
                ..Default::default()
            });
        }

        // Intermediate lines
        loop {
            // Make sure, we are not already on the last line
            start_row += 1;
            if start_row == end_row {
                break;
            }

            ctx.collection.push(SemanticToken {
                delta_line: 1,
                delta_start: 0,
                length: 2000,
                token_type,
                token_modifiers_bitset: ctx.modifiers,
                ..Default::default()
            });
        }

        // Last line
        ctx.collection.push(SemanticToken {
            delta_line: 1,
            delta_start: 0,
            length: end_col,
            token_type,
            token_modifiers_bitset: ctx.modifiers,
                ..Default::default()
        });

        ctx.last_line = start_row;
        ctx.last_start = 0;

    } else {
        let (delta_line, delta_start) = if on_same_line {
            (0, start_col - ctx.last_start)
        } else {
            (start_row - ctx.last_line, start_col)
        };

        if on_same_line {
            ctx.last_start += delta_start;
        } else {
            ctx.last_line = start_row;
            ctx.last_start = delta_start;
        }

        ctx.collection.push(SemanticToken {
            delta_line,
            delta_start,
            length: (end.column - start.column) as u32,
            token_type,
            token_modifiers_bitset: ctx.modifiers,
                ..Default::default()
        });
    }
}

