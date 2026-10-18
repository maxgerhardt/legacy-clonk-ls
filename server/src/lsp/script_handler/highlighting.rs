use tower_lsp::lsp_types::SemanticToken; use tree_sitter::{Tree, TreeCursor, Node};
use std::collections::HashSet;
use crate::lsp::{highlight_helper::{add_semantic_token, add_semantic_token_at, Context}, token_types::TokenTypes};

pub struct Highlighter;

impl Highlighter {

    #[cfg(test)]
    pub fn collect_tokens(tree: &Tree, lut: TokenTypes) -> Vec<SemanticToken> {
        Self::collect_tokens_with_source(tree, lut, "")
    }

    /// With the source text: identifiers can be told apart (engine constants, the script's static consts, `this`).
    pub fn collect_tokens_with_source(tree: &Tree, lut: TokenTypes, src: &str) -> Vec<SemanticToken> {
        let mut cursor = tree.walk();
        let mut ctx = Context {
            token_types: lut,
            ..Context::default()
        };
        let src = Source { text: src, consts: static_consts(tree, src) };

        Self::collect_tokens_step(&mut cursor, &mut ctx, &src);
        cursor.goto_first_child();

        ctx.collection
    }

    pub fn add_token(
        ctx: &mut Context,
        token_type: u32,
        node: &Node,
    ) {
        add_semantic_token(ctx, token_type, node);
    }

    fn collect_tokens_step(
        cursor: &mut TreeCursor,
        ctx: &mut Context,
        src: &Source,
    ) {
        loop {
            let node = cursor.node();
            let mut traverse_children = true;

            if !node.is_named() && matches!(node.kind(), "var" | "in")
                && node.parent().map(|p| p.kind() == "for_statement").unwrap_or(false)
            {
                Self::add_token(ctx, ctx.token_types.keyword, &node);
            }

            if node.is_named() {
                match node.kind() {
                    "comment" => {
                        Self::add_token(
                            ctx,
                            ctx.token_types.comment,
                            &node,
                        );
                    },

                    "var_definition" | "var_definition_inline" => {
                        if let Some(ref child) = node.child(0) {
                            Self::add_token(ctx, ctx.token_types.var_scope, child);
                        }

                        if let Some(ref child) = node.child_by_field_name("const") {
                            Self::add_token(ctx, ctx.token_types.keyword, child);
                        }
                    },
                    "function_definition" => {
                        let mut idx_of_func = 0;

                        if let Some(vis) = node.child_by_field_name("visibility") {
                            Self::add_token(
                                ctx,
                                ctx.token_types.keyword,
                                &vis,
                            );

                            idx_of_func += 1;
                        }

                        if let Some(name) = node.child(idx_of_func) {
                            Self::add_token(
                                ctx,
                                ctx.token_types.keyword,
                                &name,
                            );
                        }

                        if let Some(name) = node.child_by_field_name("name") {
                            Self::add_token(
                                ctx,
                                ctx.token_types.method,
                                &name,
                            );  
                        }
                    },
                    "method_call" => {

                        if let Some(child) = node.child_by_field_name("id") {
                            Self::add_token(
                                ctx,
                                ctx.token_types.id,
                                &child,
                            );

                            traverse_children = false;
                            cursor.goto_first_child();

                            if let Some(name) = node.child_by_field_name("name") {
                                Self::add_token(
                                    ctx,
                                    ctx.token_types.method,
                                    &name,
                                );

                                cursor.goto_next_sibling();
                            }

                            if cursor.goto_next_sibling() {
                                Self::collect_tokens_step(
                                    cursor,
                                    ctx,
                                    src,
                                );
                            }
                        } else {
                            if let Some(name) = node.child_by_field_name("name") {
                                Self::add_token(
                                    ctx,
                                    ctx.token_types.method,
                                    &name,
                                );
                            }
                        }
                    },
                    "string" => {
                        // $Name$ inside a string: string table reference in the macro color (single-line strings)
                        let refs = if src.text.is_empty() || node.start_position().row != node.end_position().row {
                            vec![]
                        } else {
                            crate::analysis::strings::refs(&src.text[node.start_byte()..node.end_byte()])
                        };
                        if refs.is_empty() {
                            Self::add_token(ctx, ctx.token_types.string, &node);
                        } else {
                            let row = node.start_position().row;
                            let col0 = node.start_position().column;
                            let len = node.end_byte() - node.start_byte();
                            let at = |c: usize| tree_sitter::Point { row, column: col0 + c };
                            let mut pos = 0;
                            for (_, s, e) in refs {
                                if s > pos {
                                    add_semantic_token_at(ctx, ctx.token_types.string, at(pos), at(s));
                                }
                                add_semantic_token_at(ctx, ctx.token_types.pragma_strict, at(s), at(e));
                                pos = e;
                            }
                            if pos < len {
                                add_semantic_token_at(ctx, ctx.token_types.string, at(pos), at(len));
                            }
                        }
                    },
                    "translation" => {
                        Self::add_token(ctx, ctx.token_types.pragma_strict, &node);
                    },
                    "pragma_strict" => {
                        Self::add_token(
                            ctx,
                            ctx.token_types.pragma_strict,
                            &node,
                        );
                    },
                    "param" => {
                        if let Some(child) = node.child_by_field_name("type") {
                            Self::add_token(
                                ctx,
                                ctx.token_types.parameter_type,
                                &child,
                            );
                        }

                        if let Some(child) = node.child_by_field_name("name") {
                            Self::add_token(
                                ctx,
                                ctx.token_types.parameter,
                                &child,
                            );
                        }
                    },
                    "appendto" => {
                        Self::add_token(
                            ctx,
                            ctx.token_types.appendto,
                            &node,
                        );
                    },
                    "include" => {
                        Self::add_token(
                            ctx,
                            ctx.token_types.keyword,
                            &node,
                        );
                    },
                    "number" => {
                        Self::add_token(
                            ctx,
                            ctx.token_types.number,
                            &node,
                        );
                    },
                    // nil is a constant value like true/false: their color
                    "nil" => {
                        Self::add_token(
                            ctx,
                            ctx.token_types.bool,
                            &node,
                        );
                    },
                    "builtin_constant" => {
                        Self::add_token(
                            ctx,
                            ctx.token_types.keyword,
                            &node,
                        );
                    },
                    "bool" => {
                        Self::add_token(
                            ctx,
                            ctx.token_types.bool,
                            &node,
                        );
                    },
                    "id" => {
                        Self::add_token(
                            ctx,
                            ctx.token_types.id,
                            &node,
                        );
                    },
                    "for_statement" => {
                        Self::add_token(
                            ctx,
                            ctx.token_types.keyword,
                            &node.child(0).unwrap(),
                        );
                    },
                    "while_statement" => {
                        Self::add_token(
                            ctx,
                            ctx.token_types.keyword,
                            &node.child(0).unwrap(),
                        );
                    },
                    "if_statement" => {
                        Self::add_token(
                            ctx,
                            ctx.token_types.keyword,
                            &node.child(0).unwrap(),
                        );
                    },
                    "else" => {
                        Self::add_token(
                            ctx,
                            ctx.token_types.keyword,
                            &node,
                        );
                    },
                    "return_statement" => {
                        Self::add_token(
                            ctx,
                            ctx.token_types.keyword,
                            &node.child(0).unwrap(),
                        );
                    },
                    "flow_control_statement" => {
                        Self::add_token(
                            ctx,
                            ctx.token_types.keyword,
                            &node,
                        );
                    },
                    // plain identifiers (not function names or parameters, which their parents color):
                    // `this`, engine constants and the script's static consts
                    "identifier" if !src.text.is_empty() && !Self::is_named_by_parent(&node) => {
                        let text = &src.text[node.start_byte()..node.end_byte()];
                        if text == "this" {
                            Self::add_token(ctx, ctx.token_types.this_kw, &node);
                        } else if crate::analysis::engine::ENGINE.constants.contains_key(text) || src.consts.contains(text) {
                            Self::add_token(ctx, ctx.token_types.enum_member, &node);
                        }
                    },
                    _ => {},
                }
            }

            if traverse_children && cursor.goto_first_child() {
                Self::collect_tokens_step(
                    cursor,
                    ctx,
                    src,
                );
            }

            if !cursor.goto_next_sibling() {
                break;
            }
        }

        cursor.goto_parent();
    }

}

impl Highlighter {
    /// identifiers whose parent already adds a token for them (function names, parameters)
    fn is_named_by_parent(node: &Node) -> bool {
        match node.parent() {
            Some(p) => matches!(p.kind(), "method_call" | "function_definition" | "old_function_definition" | "param"),
            None => false,
        }
    }
}

pub struct Source<'a> {
    text: &'a str,
    consts: HashSet<String>,
}

/// names of the script's `static const` declarations
fn static_consts(tree: &Tree, src: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    if src.is_empty() {
        return out;
    }
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        if n.kind() == "var_definition" && n.child_by_field_name("const").is_some() {
            let mut c = n.walk();
            for va in n.named_children(&mut c).filter(|x| x.kind() == "var_assignment") {
                if let Some(id) = va.named_child(0) {
                    out.insert(src[id.start_byte()..id.end_byte()].to_string());
                }
            }
        }
        let mut c = n.walk();
        stack.extend(n.named_children(&mut c));
    }
    out
}

#[cfg(test)]
mod tests {

    use tree_sitter::Parser;

    use super::*;

    #[test]
    fn should_have_token_for_comment() {
        let mut parser = Parser::new();
        parser.set_language(tree_sitter_c4script::language()).expect("Loading c4scrpt grammar");

        let tree = parser.parse("//comment", None).unwrap();

        let tokens = Highlighter::collect_tokens(&tree, TokenTypes::default());

        assert!(tokens.len() > 0);
    }

    #[test]
    fn should_not_crash_for_method_call_on_id() {
        let mut parser = Parser::new();
        parser.set_language(tree_sitter_c4script::language()).expect("Loading c4scrpt grammar");

        let tree = parser.parse("func GetX() { CLNK::Explode(); }", None).unwrap();

        let tokens = Highlighter::collect_tokens(&tree, TokenTypes::default());

        assert!(tokens.len() > 0);
    }

    #[test]
    fn should_not_crash_for_incomplete_method_call_on_id() {
        let mut parser = Parser::new();
        parser.set_language(tree_sitter_c4script::language()).expect("Loading c4scrpt grammar");

        let tree = parser.parse("/*-- Feurige Himmel --*/

        #strict
        
        func Initialize() 
        {
          SpreadDragons();
          CLNK::DoStuff();
        }
        ", None).unwrap();

        let tokens = Highlighter::collect_tokens(&tree, TokenTypes::default());

        assert!(tokens.len() > 0);
    }

    #[test]
    fn colors_engine_constants_static_consts_and_this() {
        let mut parser = Parser::new();
        parser.set_language(tree_sitter_c4script::language()).expect("Loading c4scrpt grammar");
        let src = "#strict 3\nstatic const MyLimit = 5;\nfunc f() { var o = CreateObject(ROCK, 0, 0, NO_OWNER); return this && MyLimit; }";
        let tree = parser.parse(src, None).unwrap();
        let lut = TokenTypes { enum_member: 20, this_kw: 21, ..TokenTypes::default() };
        let tokens = Highlighter::collect_tokens_with_source(&tree, lut, src);
        assert_eq!(tokens.iter().filter(|t| t.token_type == 20).count(), 3, "NO_OWNER, MyLimit (declaration + use)");
        assert_eq!(tokens.iter().filter(|t| t.token_type == 21).count(), 1, "this");
    }

    #[test]
    fn colors_var_in_for_loops() {
        let mut parser = Parser::new();
        parser.set_language(tree_sitter_c4script::language()).expect("Loading c4scrpt grammar");
        let src = "#strict 3\nfunc f(array a) { for (var i = 0; i < 3; i++) {} for (var x in a) {} }";
        let tree = parser.parse(src, None).unwrap();
        let lut = TokenTypes { keyword: 30, var_scope: 31, ..TokenTypes::default() };
        let tokens = Highlighter::collect_tokens_with_source(&tree, lut, src);
        // for (var i ...): var_scope; for (var x in a): var + in as keywords (besides func/for/for)
        assert_eq!(tokens.iter().filter(|t| t.token_type == 31).count(), 1, "var in for (var i = 0; ...)");
        assert_eq!(tokens.iter().filter(|t| t.token_type == 30).count(), 5, "func, for, for, var, in");
    }

    #[test]
    fn colors_string_table_refs_and_nil() {
        let mut parser = Parser::new();
        parser.set_language(tree_sitter_c4script::language()).expect("Loading c4scrpt grammar");
        let src = "#strict 3\nfunc f() { Message(\"say $TxtHello$ now\"); return nil; }";
        let tree = parser.parse(src, None).unwrap();
        let lut = TokenTypes { string: 40, pragma_strict: 41, bool: 42, ..TokenTypes::default() };
        let tokens = Highlighter::collect_tokens_with_source(&tree, lut, src);
        assert_eq!(tokens.iter().filter(|t| t.token_type == 40).count(), 2, "string before and after");
        assert_eq!(tokens.iter().filter(|t| t.token_type == 41 && t.length == 10).count(), 1, "$TxtHello$");
        assert_eq!(tokens.iter().filter(|t| t.token_type == 42).count(), 1, "nil like true/false");
    }

    #[test]
    fn should_not_crash() {
        let mut parser = Parser::new();
        parser.set_language(tree_sitter_c4script::language()).expect("Loading c4scrpt grammar");

        let tree = parser.parse("func GetX() { Explode(100); }", None).unwrap();
        let tokens = Highlighter::collect_tokens(&tree, TokenTypes::default());

        assert!(tokens.len() > 0);
    }
}
