use tower_lsp::lsp_types::SemanticToken; use tree_sitter::{Tree, TreeCursor, Node};
use std::cell::RefCell;
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
        let src = Source {
            text: src,
            consts: static_consts(tree, src),
            fields: object_vars(tree, src),
            own_funcs: own_functions(tree, src),
            locals: RefCell::new(HashSet::new()),
            params: RefCell::new(HashSet::new()),
        };

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
                let t = if node.kind() == "in" { ctx.token_types.control_kw } else { ctx.token_types.keyword };
                Self::add_token(ctx, t, &node);
            }

            // entering a function: its parameters and var-declared locals (C4Script vars are function-scoped)
            if matches!(node.kind(), "function_definition" | "old_function_definition") && !src.text.is_empty() {
                let (params, locals) = function_names(&node, src.text);
                *src.params.borrow_mut() = params;
                *src.locals.borrow_mut() = locals;
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
                                ctx.modifiers = if is_engine_function(&name, src) { 1 } else { 0 };
                                Self::add_token(
                                    ctx,
                                    ctx.token_types.method,
                                    &name,
                                );
                                ctx.modifiers = 0;
                            }
                        }
                    },
                    "string" => {
                        // pieces inside a single-line string: $Name$ string table references (macro color), format
                        // placeholders %d %s ... (only in the format string of Format/Log/Message/...), escapes \" \\
                        let pieces = if src.text.is_empty() || node.start_position().row != node.end_position().row {
                            vec![]
                        } else {
                            string_pieces(&node, src.text, &ctx.token_types)
                        };
                        if pieces.is_empty() {
                            Self::add_token(ctx, ctx.token_types.string, &node);
                        } else {
                            let row = node.start_position().row;
                            let col0 = node.start_position().column;
                            let len = node.end_byte() - node.start_byte();
                            let at = |c: usize| tree_sitter::Point { row, column: col0 + c };
                            let mut pos = 0;
                            for (s, e, t) in pieces {
                                if s > pos {
                                    add_semantic_token_at(ctx, ctx.token_types.string, at(pos), at(s));
                                }
                                add_semantic_token_at(ctx, t, at(s), at(e));
                                pos = e;
                            }
                            if pos < len {
                                add_semantic_token_at(ctx, ctx.token_types.string, at(pos), at(len));
                            }
                        }
                    },
                    "translation" => {
                        Self::add_token(ctx, ctx.token_types.translation, &node);
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
                            ctx.token_types.control_kw,
                            &node.child(0).unwrap(),
                        );
                    },
                    "while_statement" => {
                        Self::add_token(
                            ctx,
                            ctx.token_types.control_kw,
                            &node.child(0).unwrap(),
                        );
                    },
                    "if_statement" => {
                        Self::add_token(
                            ctx,
                            ctx.token_types.control_kw,
                            &node.child(0).unwrap(),
                        );
                    },
                    "else" => {
                        Self::add_token(
                            ctx,
                            ctx.token_types.control_kw,
                            &node,
                        );
                    },
                    "return_statement" => {
                        Self::add_token(
                            ctx,
                            ctx.token_types.control_kw,
                            &node.child(0).unwrap(),
                        );
                    },
                    "flow_control_statement" => {
                        Self::add_token(
                            ctx,
                            ctx.token_types.control_kw,
                            &node,
                        );
                    },
                    // plain identifiers (not function names or parameters, which their parents color):
                    // `this`, engine constants and the script's static consts
                    "identifier" if !src.text.is_empty() && !Self::is_named_by_parent(&node) => {
                        let text = &src.text[node.start_byte()..node.end_byte()];
                        if text == "this" {
                            Self::add_token(ctx, ctx.token_types.this_kw, &node);
                        } else if is_map_key(&node) {
                            // map keys {speed = 5} and m.speed: properties, like JavaScript object keys - never the
                            // local variable of the same name
                            Self::add_token(ctx, ctx.token_types.property, &node);
                        } else if src.params.borrow().contains(text) {
                            Self::add_token(ctx, ctx.token_types.parameter, &node);
                        } else if src.locals.borrow().contains(text) {
                            Self::add_token(ctx, ctx.token_types.variable, &node);
                        } else if crate::analysis::engine::ENGINE.constants.contains_key(text) || src.consts.contains(text) {
                            Self::add_token(ctx, ctx.token_types.enum_member, &node);
                        } else if src.fields.contains(text) {
                            Self::add_token(ctx, ctx.token_types.property, &node);
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
    /// object locals and statics of the script
    fields: HashSet<String>,
    /// functions defined in this script (a call to one of them isn't an engine call)
    own_funcs: HashSet<String>,
    /// parameters and var-declared locals of the function being walked
    locals: RefCell<HashSet<String>>,
    params: RefCell<HashSet<String>>,
}

/// Engine functions whose string argument is a format string (C4Script.cpp FnStringFormat callers) and its index.
const FORMAT_FUNCS: [(&str, usize); 7] =
    [("Format", 0), ("Log", 0), ("DebugLog", 0), ("Message", 0), ("AddMessage", 0), ("PlrMessage", 0), ("PlayerMessage", 1)];

/// Sub-ranges of a string node (byte offsets relative to the node) with their token type, sorted.
fn string_pieces(node: &Node, src: &str, tt: &TokenTypes) -> Vec<(usize, usize, u32)> {
    let text = &src[node.start_byte()..node.end_byte()];
    let mut out: Vec<(usize, usize, u32)> = crate::analysis::strings::refs(text).into_iter().map(|(_, s, e)| (s, e, tt.translation)).collect();
    let b = text.as_bytes();
    // escapes the engine knows: \" and \\ (C4AulParse)
    let mut i = 1;
    while i + 1 < b.len() {
        if b[i] == b'\\' && (b[i + 1] == b'"' || b[i + 1] == b'\\') {
            out.push((i, i + 2, tt.escape));
            i += 2;
        } else {
            i += 1;
        }
    }
    if is_format_string(node, src) {
        // %[digits.]*[dxXcivs%] (C4Script.cpp FnStringFormat)
        let mut i = 1;
        while i < b.len() {
            if b[i] == b'%' {
                let mut j = i + 1;
                while j < b.len() && (b[j].is_ascii_digit() || b[j] == b'.') {
                    j += 1;
                }
                if j < b.len() && matches!(b[j], b'd' | b'x' | b'X' | b'c' | b'i' | b'v' | b's' | b'%') {
                    out.push((i, j + 1, tt.placeholder));
                    i = j + 1;
                    continue;
                }
            }
            i += 1;
        }
    }
    out.sort();
    // drop overlaps (a $Name$ wins over anything inside it)
    let mut kept: Vec<(usize, usize, u32)> = vec![];
    for p in out {
        if kept.last().map(|l| p.0 < l.1).unwrap_or(false) {
            continue;
        }
        kept.push(p);
    }
    kept
}

fn is_format_string(node: &Node, src: &str) -> bool {
    let Some(args) = node.parent().filter(|p| p.kind() == "args_list") else { return false };
    let Some(call) = args.parent().filter(|p| p.kind() == "method_call") else { return false };
    let Some(name) = call.child_by_field_name("name") else { return false };
    let name = &src[name.start_byte()..name.end_byte()];
    let mut c = args.walk();
    let index = args.named_children(&mut c).filter(|x| x.kind() != "comment").position(|x| x.id() == node.id());
    FORMAT_FUNCS.iter().any(|(f, i)| *f == name && Some(*i) == index)
}

/// parameters and var-declared locals of a function
fn function_names(func: &Node, src: &str) -> (HashSet<String>, HashSet<String>) {
    let (mut params, mut locals) = (HashSet::new(), HashSet::new());
    let mut stack = vec![*func];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "param" => {
                if let Some(x) = n.child_by_field_name("name") {
                    params.insert(src[x.start_byte()..x.end_byte()].to_string());
                }
            }
            "var_assignment" => {
                let is_var = n.parent().map(|p| {
                    let mut c = p.walk();
                    let r = p.named_children(&mut c).any(|x| x.kind() == "var_scope" && &src[x.start_byte()..x.end_byte()] == "var");
                    r
                }).unwrap_or(false);
                if is_var {
                    if let Some(x) = n.named_child(0) {
                        locals.insert(src[x.start_byte()..x.end_byte()].to_string());
                    }
                }
            }
            "for_statement" => {
                // for (var x in a): the loop variables are plain identifier children
                let mut c = n.walk();
                let kids: Vec<Node> = n.children(&mut c).collect();
                if kids.iter().any(|k| k.kind() == "in") && kids.iter().any(|k| k.kind() == "var") {
                    for k in kids.iter().take_while(|k| k.kind() != "in").filter(|k| k.kind() == "identifier") {
                        locals.insert(src[k.start_byte()..k.end_byte()].to_string());
                    }
                }
            }
            _ => {}
        }
        let mut c = n.walk();
        stack.extend(n.named_children(&mut c));
    }
    (params, locals)
}

/// the key of a map entry ({speed = 5}) or of a map access with '.' (m.speed)
fn is_map_key(node: &Node) -> bool {
    match node.parent() {
        Some(p) if p.kind() == "map_key" => true,
        Some(p) if p.kind() == "map_access" => {
            // m.speed: the identifier right after the '.' (not m[speed], where speed is an expression)
            node.prev_sibling().map(|s| s.kind() == ".").unwrap_or(false)
        }
        _ => false,
    }
}

/// an engine function (not overridden by a function of this script): semantic modifier defaultLibrary
fn is_engine_function(name: &Node, src: &Source) -> bool {
    if src.text.is_empty() {
        return false;
    }
    let n = &src.text[name.start_byte()..name.end_byte()];
    crate::analysis::engine::ENGINE.functions.contains_key(n) && !src.own_funcs.contains(n)
}

/// names of the functions defined in the script
fn own_functions(tree: &Tree, src: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    if src.is_empty() {
        return out;
    }
    let root = tree.root_node();
    let mut c = root.walk();
    for f in root.named_children(&mut c).filter(|n| matches!(n.kind(), "function_definition" | "old_function_definition")) {
        if let Some(n) = f.child_by_field_name("name") {
            out.insert(src[n.start_byte()..n.end_byte()].to_string());
        }
    }
    out
}

/// names of the script's object locals and statics (not consts)
fn object_vars(tree: &Tree, src: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    if src.is_empty() {
        return out;
    }
    let root = tree.root_node();
    let mut c = root.walk();
    for n in root.named_children(&mut c).filter(|n| n.kind() == "var_definition" && n.child_by_field_name("const").is_none()) {
        let mut c2 = n.walk();
        let scope_ok = n.named_children(&mut c2).any(|x| x.kind() == "var_scope" && matches!(&src[x.start_byte()..x.end_byte()], "local" | "static"));
        if !scope_ok {
            continue;
        }
        let mut c3 = n.walk();
        for va in n.named_children(&mut c3).filter(|x| x.kind() == "var_assignment") {
            if let Some(id) = va.named_child(0) {
                out.insert(src[id.start_byte()..id.end_byte()].to_string());
            }
        }
    }
    out
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
        let lut = TokenTypes { keyword: 30, var_scope: 31, control_kw: 32, ..TokenTypes::default() };
        let tokens = Highlighter::collect_tokens_with_source(&tree, lut, src);
        // for (var i ...): var_scope; for (var x in a): var + in as keywords (besides func/for/for)
        assert_eq!(tokens.iter().filter(|t| t.token_type == 31).count(), 1, "var in for (var i = 0; ...)");
        assert_eq!(tokens.iter().filter(|t| t.token_type == 30).count(), 2, "func, var (of for-in)");
        assert_eq!(tokens.iter().filter(|t| t.token_type == 32).count(), 3, "for, for, in: control keywords");
    }

    #[test]
    fn colors_string_table_refs_and_nil() {
        let mut parser = Parser::new();
        parser.set_language(tree_sitter_c4script::language()).expect("Loading c4scrpt grammar");
        let src = "#strict 3\nfunc f() { Message(\"say $TxtHello$ now\"); return nil; }";
        let tree = parser.parse(src, None).unwrap();
        let lut = TokenTypes { string: 40, translation: 41, bool: 42, ..TokenTypes::default() };
        let tokens = Highlighter::collect_tokens_with_source(&tree, lut, src);
        assert_eq!(tokens.iter().filter(|t| t.token_type == 40).count(), 2, "string before and after");
        assert_eq!(tokens.iter().filter(|t| t.token_type == 41 && t.length == 10).count(), 1, "$TxtHello$");
        assert_eq!(tokens.iter().filter(|t| t.token_type == 42).count(), 1, "nil like true/false");
    }

    #[test]
    fn colors_like_csharp() {
        let mut parser = Parser::new();
        parser.set_language(tree_sitter_c4script::language()).expect("Loading c4scrpt grammar");
        let src = "#strict 3\nlocal count;\nfunc f(int k) { var x = k; if (x) return Format(\"%d \\\" %5s\", x, count); else x = 1; for (var i in [1]) break; }";
        let tree = parser.parse(src, None).unwrap();
        let lut = TokenTypes { control_kw: 50, variable: 51, property: 52, placeholder: 53, escape: 54, parameter: 55, ..TokenTypes::default() };
        let tokens = Highlighter::collect_tokens_with_source(&tree, lut, src);
        let n = |t: u32| tokens.iter().filter(|x| x.token_type == t).count();
        assert_eq!(n(50), 6, "if, return, else, for, in, break");
        assert_eq!(n(51), 5, "x (declaration + 3 uses), i");
        assert_eq!(n(52), 2, "count: declaration + use");
        assert_eq!(n(53), 2, "%d, %5s");
        assert_eq!(n(54), 1, "\\\"");
        assert_eq!(n(55), 2, "k: declaration + use");
    }

    #[test]
    fn engine_functions_get_default_library() {
        let mut parser = Parser::new();
        parser.set_language(tree_sitter_c4script::language()).expect("Loading c4scrpt grammar");
        let src = "#strict 3\nfunc Mine() { return 1; }\nfunc f() { Mine(); GetX(); }";
        let tree = parser.parse(src, None).unwrap();
        let lut = TokenTypes { method: 60, ..TokenTypes::default() };
        let tokens = Highlighter::collect_tokens_with_source(&tree, lut, src);
        let calls: Vec<u32> = tokens.iter().filter(|t| t.token_type == 60).map(|t| t.token_modifiers_bitset).collect();
        // Mine (definition), f (definition), Mine() call, GetX() call
        assert_eq!(calls, vec![0, 0, 0, 1]);
    }

    #[test]
    fn map_keys_are_properties_like_js_objects() {
        let mut parser = Parser::new();
        parser.set_language(tree_sitter_c4script::language()).expect("Loading c4scrpt grammar");
        // speed is also a local variable (declared later): the key and the .speed access stay properties
        let src = "#strict 3
func f() { var m = {speed = 5, name = 1}; var speed = m.speed; return speed; }";
        let tree = parser.parse(src, None).unwrap();
        let lut = TokenTypes { variable: 70, property: 71, ..TokenTypes::default() };
        let tokens = Highlighter::collect_tokens_with_source(&tree, lut, src);
        let n = |t: u32| tokens.iter().filter(|x| x.token_type == t).count();
        assert_eq!(n(71), 3, "keys speed, name and the .speed access");
        assert_eq!(n(70), 4, "m (2x), speed (declaration + return)");
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
