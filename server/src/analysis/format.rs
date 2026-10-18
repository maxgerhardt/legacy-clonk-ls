//! C4Script formatter (c4format, LSP textDocument/formatting), like clang-format with a project rules file.
//!
//! The rules come from `.c4format` in the script's folder or any parent folder (`key: value` lines, `#` comments):
//!   indent_width: 2            use_tabs: false
//!   function_brace: same_line  (or next_line)    control_brace: same_line  (or next_line, for if/else/while/for)
//!   else_on_new_line: false    require_braces: true (if/else/while/for bodies always get { })
//!   max_blank_lines: 1         space_after_control_keyword: true  (if (x) instead of if(x))
//!   align_trailing_comments: true   uniform_block_comments: true (/* ... */ lines get " * ")
//!   header_comment: true       (/* -- Name -- */ -> /*-- Name --*/)
//!   short_functions: true      (func F() { return 1; } written on one line stays on one line)
//!
//! How it works: the script's tokens (leaves of the syntax tree; strings, comments, `[...]` descriptions and
//! `#strict N` stay whole) are laid out again. Line breaks stay where they were, except around braces, which the
//! rules decide; blank lines are limited. Indentation comes from the open braces/brackets. Spaces follow the rules
//! for the token pairs they know (operators, commas, calls, keywords ...) and are kept as they were otherwise.
//!
//! Safety: the result is parsed again; it must have no syntax errors, the same tokens (plus the braces added by
//! require_braces) and the same comment text, else the script is left unchanged (None). Scripts without #strict,
//! with old-style `Name:` functions or with syntax errors are never formatted.

use super::index::new_parser;
use super::text::node_text;
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Tree};

#[derive(Clone, Debug, PartialEq)]
pub struct Style {
    pub indent_width: usize,
    pub use_tabs: bool,
    pub function_brace_next_line: bool,
    pub control_brace_next_line: bool,
    pub else_on_new_line: bool,
    pub require_braces: bool,
    pub max_blank_lines: usize,
    pub space_after_control_keyword: bool,
    pub align_trailing_comments: bool,
    pub uniform_block_comments: bool,
    pub header_comment: bool,
    /// a function written on one line with a single simple statement stays on one line
    pub short_functions: bool,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            indent_width: 2,
            use_tabs: false,
            function_brace_next_line: false,
            control_brace_next_line: false,
            else_on_new_line: false,
            require_braces: true,
            max_blank_lines: 1,
            space_after_control_keyword: true,
            align_trailing_comments: true,
            uniform_block_comments: true,
            header_comment: true,
            short_functions: true,
        }
    }
}

pub const STYLE_FILE: &str = ".c4format";

impl Style {
    /// Parse a `.c4format` file (unknown keys and bad values are errors, so typos don't go unnoticed).
    pub fn parse(text: &str) -> Result<Style, String> {
        let mut s = Style::default();
        for (n, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let (k, v) = line.split_once(':').ok_or(format!("line {}: expected key: value", n + 1))?;
            let (k, v) = (k.trim(), v.trim());
            let flag = |v: &str| match v {
                "true" => Ok(true),
                "false" => Ok(false),
                _ => Err(format!("line {}: {} must be true or false", n + 1, k)),
            };
            let brace = |v: &str| match v {
                "same_line" => Ok(false),
                "next_line" => Ok(true),
                _ => Err(format!("line {}: {} must be same_line or next_line", n + 1, k)),
            };
            let num = |v: &str| v.parse::<usize>().map_err(|_| format!("line {}: {} must be a number", n + 1, k));
            match k {
                "indent_width" => s.indent_width = num(v)?,
                "use_tabs" => s.use_tabs = flag(v)?,
                "function_brace" => s.function_brace_next_line = brace(v)?,
                "control_brace" => s.control_brace_next_line = brace(v)?,
                "else_on_new_line" => s.else_on_new_line = flag(v)?,
                "require_braces" => s.require_braces = flag(v)?,
                "max_blank_lines" => s.max_blank_lines = num(v)?,
                "space_after_control_keyword" => s.space_after_control_keyword = flag(v)?,
                "align_trailing_comments" => s.align_trailing_comments = flag(v)?,
                "uniform_block_comments" => s.uniform_block_comments = flag(v)?,
                "header_comment" => s.header_comment = flag(v)?,
                "short_functions" => s.short_functions = flag(v)?,
                _ => return Err(format!("line {}: unknown key {}", n + 1, k)),
            }
        }
        Ok(s)
    }

    /// The rules for a script: the nearest `.c4format` in its folder or a parent folder, else the defaults.
    pub fn for_file(script: &Path) -> Result<(Style, Option<PathBuf>), String> {
        let mut dir = script.parent();
        while let Some(d) = dir {
            let f = d.join(STYLE_FILE);
            if f.is_file() {
                let text = std::fs::read_to_string(&f).map_err(|e| format!("{}: {}", f.display(), e))?;
                return Style::parse(&text).map(|s| (s, Some(f.clone()))).map_err(|e| format!("{}: {}", f.display(), e));
            }
            dir = d.parent();
        }
        Ok((Style::default(), None))
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Role {
    Plain,
    /// `{` of a function body / of an if/else/while/for body / of another block
    FuncOpen,
    CtrlOpen,
    BlockOpen,
    BlockClose,
    MapOpen,
    MapClose,
    /// operator of a binary expression, `=` of a var declaration or map entry
    Binary,
    /// prefix / postfix operator of a unary expression
    Prefix,
    Postfix,
}

#[derive(Clone, Debug)]
struct Tok {
    text: String,
    kind: String,
    parent: String,
    role: Role,
    /// line breaks / whitespace in the original between the previous token and this one
    nl_before: usize,
    space_before: bool,
    /// added by require_braces
    virt: bool,
    /// `{` / `}` of a short function kept on one line
    inline: bool,
}

impl Tok {
    fn is_comment(&self) -> bool {
        self.kind == "comment"
    }
    fn is_line_comment(&self) -> bool {
        self.kind == "comment" && self.text.starts_with("//")
    }
    fn opens_block(&self) -> bool {
        matches!(self.role, Role::FuncOpen | Role::CtrlOpen | Role::BlockOpen)
    }
}

fn atomic(kind: &str) -> bool {
    matches!(kind, "string" | "comment" | "func_desc" | "pragma_strict" | "translation" | "number" | "id")
}

fn collect<'a>(n: Node<'a>, out: &mut Vec<Node<'a>>) {
    if n.child_count() == 0 || atomic(n.kind()) {
        out.push(n);
        return;
    }
    let mut c = n.walk();
    for ch in n.children(&mut c) {
        collect(ch, out);
    }
}

fn role_of(n: &Node) -> Role {
    let Some(p) = n.parent() else { return Role::Plain };
    let t = n.kind();
    match (p.kind(), t) {
        ("block", "{") => match p.parent().map(|g| g.kind()) {
            Some("function_definition") => Role::FuncOpen,
            Some("if_statement") | Some("while_statement") | Some("for_statement") => Role::CtrlOpen,
            _ => Role::BlockOpen,
        },
        ("block", "}") => Role::BlockClose,
        ("map", "{") => Role::MapOpen,
        ("map", "}") => Role::MapClose,
        ("binary_expression", op) if !n.is_named() && op != "(" && op != ")" => Role::Binary,
        ("var_assignment", "=") | ("map_entry", "=") => Role::Binary,
        ("unary_expression", op) if !n.is_named() && op != "(" && op != ")" => {
            if n.prev_sibling().is_none() { Role::Prefix } else { Role::Postfix }
        }
        _ => Role::Plain,
    }
}

/// `{` or `}` of a function body written on one line with exactly one statement that isn't if/while/for/a block
fn short_function_brace(n: &Node) -> bool {
    if !matches!(n.kind(), "{" | "}") {
        return false;
    }
    let Some(block) = n.parent().filter(|p| p.kind() == "block") else { return false };
    if !block.parent().map(|f| f.kind() == "function_definition").unwrap_or(false) {
        return false;
    }
    if block.start_position().row != block.end_position().row {
        return false;
    }
    let mut c = block.walk();
    let inner: Vec<Node> = block.children(&mut c).filter(|k| !matches!(k.kind(), "{" | "}")).collect();
    // one statement: a named node (return/var/...) or an expression + ';' (hidden expression statement)
    let simple = match inner.as_slice() {
        [one] => one.is_named() && !matches!(one.kind(), "if_statement" | "while_statement" | "for_statement" | "block" | "comment"),
        [expr, semi] => expr.is_named() && expr.kind() != "comment" && semi.kind() == ";",
        _ => false,
    };
    simple
}

/// Bodies of if/else/while/for that aren't blocks: (start byte, end byte). An expression statement is a hidden
/// grammar rule, so a body like `F();` is two children of the statement (the expression and the `;`).
fn unbraced_bodies(root: Node, out: &mut Vec<(usize, usize)>) {
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if matches!(n.kind(), "if_statement" | "while_statement" | "for_statement") {
            let kids: Vec<Node> = {
                let mut c = n.walk();
                n.children(&mut c).filter(|k| k.kind() != "comment").collect()
            };
            // the ')' that closes the header's '('
            let mut depth = 0;
            let mut close = None;
            for (i, k) in kids.iter().enumerate() {
                match k.kind() {
                    "(" => depth += 1,
                    ")" => {
                        depth -= 1;
                        if depth == 0 {
                            close = Some(i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(close) = close {
                let else_at = kids.iter().position(|k| k.kind() == "else");
                let mut segments = vec![(close + 1, else_at.unwrap_or(kids.len()))];
                if let Some(e) = else_at {
                    segments.push((e + 1, kids.len()));
                }
                for (from, to) in segments {
                    let seg = &kids[from.min(to)..to];
                    let Some(first) = seg.first() else { continue };
                    let last = seg.last().unwrap();
                    let single_kind = if seg.len() == 1 { first.kind() } else { "" };
                    if single_kind == "block" || single_kind == ";" || single_kind == "if_statement" && Some(from) != Some(close + 1) {
                        continue; // already a block, an empty statement, or else-if
                    }
                    out.push((first.start_byte(), last.end_byte()));
                }
            }
        }
        let mut c = n.walk();
        stack.extend(n.named_children(&mut c));
    }
}

fn tokens(tree: &Tree, src: &str, style: &Style) -> Vec<Tok> {
    let mut leaves = vec![];
    collect(tree.root_node(), &mut leaves);
    let mut out: Vec<Tok> = vec![];
    let mut prev_end = 0;
    for n in &leaves {
        let gap = &src[prev_end.min(n.start_byte())..n.start_byte()];
        out.push(Tok {
            text: node_text(n, src).to_string(),
            kind: n.kind().to_string(),
            parent: n.parent().map(|p| p.kind().to_string()).unwrap_or_default(),
            role: role_of(n),
            nl_before: gap.matches('\n').count(),
            space_before: !gap.is_empty(),
            virt: false,
            inline: style.short_functions && short_function_brace(n),
        });
        prev_end = n.end_byte();
    }
    if !style.require_braces {
        return out;
    }
    // insert { } around unbraced bodies: before the first token of the body, after its last one (or after a line
    // comment right behind it on the same line)
    let mut bodies = vec![];
    unbraced_bodies(tree.root_node(), &mut bodies);
    if bodies.is_empty() {
        return out;
    }
    let starts: Vec<usize> = leaves.iter().map(|n| n.start_byte()).collect();
    let ends: Vec<usize> = leaves.iter().map(|n| n.end_byte()).collect();
    let mut open_before = vec![0usize; out.len()];
    let mut close_after = vec![0usize; out.len()];
    for (s, e) in bodies {
        if let Some(i) = starts.iter().position(|&x| x == s) {
            open_before[i] += 1;
        }
        if let Some(mut j) = ends.iter().rposition(|&x| x == e) {
            if j + 1 < out.len() && out[j + 1].is_line_comment() && out[j + 1].nl_before == 0 {
                j += 1;
            }
            close_after[j] += 1;
        }
    }
    let virt = |text: &str, role: Role| Tok {
        text: text.to_string(),
        kind: text.to_string(),
        parent: "block".to_string(),
        role,
        nl_before: 0,
        space_before: true,
        virt: true,
        inline: false,
    };
    let mut res = vec![];
    for (i, t) in out.into_iter().enumerate() {
        for _ in 0..open_before[i] {
            res.push(virt("{", Role::CtrlOpen));
        }
        res.push(t);
        for _ in 0..close_after[i] {
            res.push(virt("}", Role::BlockClose));
        }
    }
    res
}

const CONTROL_KEYWORDS: [&str; 3] = ["if", "while", "for"];

fn wordish(s: &str) -> bool {
    s.chars().last().map(|c| c.is_alphanumeric() || c == '_' || c == '"').unwrap_or(false)
}
fn wordish_start(s: &str) -> bool {
    s.chars().next().map(|c| c.is_alphanumeric() || c == '_' || c == '"' || c == '#').unwrap_or(false)
}

/// Should there be a space between a and b on the same line?
fn space(a: &Tok, b: &Tok, style: &Style) -> bool {
    let (x, y) = (a.text.as_str(), b.text.as_str());
    if b.is_comment() || a.is_comment() {
        return true;
    }
    if y == "," || y == ";" {
        return false;
    }
    if x == "," || x == ";" {
        return true;
    }
    if y == ")" || y == "]" || x == "(" || x == "[" {
        return false;
    }
    for t in ["->", "::", "."] {
        if x == t || y == t {
            return false;
        }
    }
    // nil-safe ?-> ?. ?[ and the fail-safe ->~
    if (x == "?" || y == "?") && matches!(b.parent.as_str(), "arrow_expression" | "map_access") || (x == "?" && matches!(a.parent.as_str(), "arrow_expression" | "map_access")) {
        return false;
    }
    if x == "~" && a.parent == "arrow_expression" {
        return false;
    }
    if a.role == Role::Prefix || b.role == Role::Postfix {
        return false;
    }
    if y == "(" {
        if matches!(b.parent.as_str(), "args_list" | "parameter_list") {
            return false; // F(x), func F(x)
        }
        if CONTROL_KEYWORDS.contains(&x) {
            return style.space_after_control_keyword;
        }
        if a.role == Role::Binary {
            return true;
        }
        // return(x) (old idiom) and return (x) both stay as written
        return b.space_before;
    }
    if y == "[" && matches!(b.parent.as_str(), "map_access" | "array_append") {
        return false;
    }
    if a.role == Role::Binary || b.role == Role::Binary {
        return true;
    }
    if b.opens_block() || b.role == Role::MapOpen && a.role == Role::Binary {
        return true;
    }
    if a.inline && a.opens_block() || b.inline && b.role == Role::BlockClose {
        return true; // { return 1; }
    }
    if x == "}" && y == "else" {
        return true;
    }
    if wordish(x) && wordish_start(y) {
        return true; // keywords, names, numbers next to each other always need one
    }
    b.space_before
}

struct Line {
    level: usize,
    code: String,
    trailing: Option<String>,
    /// raw lines of a reformatted block comment that follow this line (already indented)
    raw: bool,
}

/// Format a script; None = leave it unchanged (old syntax, syntax errors, or the safety check failed).
pub fn format(src: &str, style: &Style) -> Option<String> {
    format_explain(src, style).ok()
}

/// Like format, with the reason when the script is left unchanged.
pub fn format_explain(src: &str, style: &Style) -> Result<String, &'static str> {
    let mut parser = new_parser();
    let tree = parser.parse(src, None).ok_or("parser failed")?;
    let root = tree.root_node();
    if root.has_error() {
        return Err("syntax errors");
    }
    if super::diagnostics::strict_level(&root, src) == 0 {
        return Err("no #strict (old syntax)");
    }
    {
        let mut c = root.walk();
        if root.named_children(&mut c).any(|n| n.kind() == "old_function_definition") {
            return Err("old-style Name: functions");
        }
    }
    let toks = tokens(&tree, src, style);
    let text = render(&toks, style, src.contains("\r\n"));
    // safety: same tokens (incl. the added braces) and comment text, no syntax errors
    let new_tree = parser.parse(&text, None).ok_or("parser failed")?;
    if new_tree.root_node().has_error() {
        return Err("safety: the result has syntax errors");
    }
    let mut new_leaves = vec![];
    collect(new_tree.root_node(), &mut new_leaves);
    let code = |v: Vec<(String, bool)>| v.into_iter().filter(|(_, c)| !*c).map(|(t, _)| t).collect::<Vec<_>>();
    let comments = |v: Vec<(String, bool)>| {
        v.into_iter().filter(|(_, c)| *c).map(|(t, _)| t.chars().filter(|c| !c.is_whitespace() && *c != '*' && *c != '-').collect::<String>()).collect::<Vec<_>>()
    };
    let before: Vec<(String, bool)> = toks.iter().map(|t| (t.text.clone(), t.is_comment())).collect();
    let after: Vec<(String, bool)> = new_leaves.iter().map(|n| (node_text(n, &text).to_string(), n.kind() == "comment")).collect();
    if code(before.clone()) != code(after.clone()) {
        return Err("safety: tokens differ");
    }
    // structure: the same tree once the braces of if/else/while/for bodies are removed (an added brace in the
    // wrong place would move a statement into or out of a branch)
    if shape(&root, src) != shape(&new_tree.root_node(), &text) {
        return Err("safety: structure differs");
    }
    if comments(before) != comments(after) {
        return Err("safety: comment text differs");
    }
    Ok(text)
}

/// The syntax tree as text without comments, with the blocks of if/else/while/for bodies flattened into the
/// statement (so `if (x) A;` and `if (x) { A; }` look the same, but which branch a statement is in still counts).
fn shape(n: &Node, src: &str) -> String {
    let mut out = String::new();
    shape_into(n, src, &mut out);
    out
}

fn shape_into(n: &Node, src: &str, out: &mut String) {
    if n.kind() == "comment" {
        return;
    }
    if n.child_count() == 0 || atomic(n.kind()) {
        out.push_str(node_text(n, src));
        out.push(' ');
        return;
    }
    let body_block = n.kind() == "block"
        && n.parent().map(|p| matches!(p.kind(), "if_statement" | "while_statement" | "for_statement")).unwrap_or(false);
    if !body_block {
        out.push('(');
        out.push_str(n.kind());
        out.push(' ');
    }
    let mut c = n.walk();
    for ch in n.children(&mut c) {
        if body_block && matches!(ch.kind(), "{" | "}") {
            continue;
        }
        shape_into(&ch, src, out);
    }
    if !body_block {
        out.push_str(") ");
    }
}

fn render(toks: &[Tok], style: &Style, crlf: bool) -> String {
    let mut lines: Vec<Line> = vec![];
    let mut cur = Line { level: 0, code: String::new(), trailing: None, raw: false };
    // open braces/brackets: (closer, indent levels it adds)
    let mut stack: Vec<(&str, usize)> = vec![];
    let mut prev: Option<&Tok> = None;
    let mut prev_code: Option<&Tok> = None;
    let mut force_nl = false;
    for (i, t) in toks.iter().enumerate() {
        let nl = match prev {
            None => 0,
            Some(p) => newlines(p, t, toks.get(i.wrapping_sub(1)), style, force_nl),
        };
        force_nl = false;
        let closer = matches!(t.text.as_str(), ")" | "]") || matches!(t.role, Role::BlockClose | Role::MapClose);
        if nl > 0 && prev.is_some() {
            lines.push(std::mem::replace(&mut cur, Line { level: 0, code: String::new(), trailing: None, raw: false }));
            for _ in 1..nl.min(style.max_blank_lines + 1) {
                lines.push(Line { level: 0, code: String::new(), trailing: None, raw: false });
            }
        }
        if cur.code.is_empty() && cur.trailing.is_none() {
            // indentation of a new line
            let mut level: usize = stack.iter().map(|(_, l)| l).sum();
            if closer {
                level = level.saturating_sub(stack.last().map(|(_, l)| *l).unwrap_or(0));
            }
            let innermost_block = stack.last().map(|(c, _)| *c == "}").unwrap_or(true);
            let continues = prev_code.map(|p| !ends_statement(p)).unwrap_or(false)
                && innermost_block
                && !closer
                && !t.opens_block()
                && t.text != "else"
                && !t.is_comment()
                && !stack.last().map(|(_, l)| *l == 1 && t.role == Role::Plain && prev_code.map(|p| p.text == ",").unwrap_or(false)).unwrap_or(false);
            if continues && !stack.iter().any(|(c, _)| *c == "map}") {
                level += 2;
            }
            cur.level = level;
        } else if prev.map(|p| space(p, t, style)).unwrap_or(false) {
            cur.code.push(' ');
        }
        // the token
        if t.is_line_comment() && nl == 0 && !cur.code.is_empty() {
            cur.code = cur.code.trim_end().to_string();
            cur.trailing = Some(t.text.clone());
            force_nl = true;
        } else if t.is_comment() && t.text.starts_with("/*") {
            let text = comment_text(&t.text, style);
            let mut parts = text.split('\n');
            cur.code.push_str(parts.next().unwrap_or(""));
            for part in parts {
                let level = cur.level;
                lines.push(std::mem::replace(&mut cur, Line { level, code: String::new(), trailing: None, raw: true }));
                cur.code.push_str(part);
            }
            if text.contains('\n') {
                force_nl = true;
            }
        } else {
            cur.code.push_str(&t.text);
            if t.is_line_comment() {
                force_nl = true;
            }
        }
        // brackets
        match t.text.as_str() {
            "(" => stack.push((")", 2)),
            "[" => stack.push(("]", 1)),
            "{" if t.role == Role::MapOpen => stack.push(("map}", 1)),
            "{" => stack.push(("}", 1)),
            ")" | "]" | "}" => {
                stack.pop();
            }
            _ => {}
        }
        prev = Some(t);
        if !t.is_comment() {
            prev_code = Some(t);
        }
    }
    lines.push(cur);
    // trailing comments aligned per group of consecutive lines
    let indent = |level: usize| if style.use_tabs { "\t".repeat(level) } else { " ".repeat(level * style.indent_width) };
    let mut out_lines: Vec<String> = vec![];
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trailing.is_some() && style.align_trailing_comments {
            let mut j = i;
            while j < lines.len() && lines[j].trailing.is_some() {
                j += 1;
            }
            let width = (i..j).map(|k| indent(lines[k].level).len() + lines[k].code.len()).max().unwrap_or(0);
            for k in i..j {
                let head = format!("{}{}", indent(lines[k].level), lines[k].code);
                out_lines.push(format!("{}{} {}", head, " ".repeat(width - head.len()), lines[k].trailing.as_ref().unwrap()));
            }
            i = j;
            continue;
        }
        let l = &lines[i];
        let head = if l.code.is_empty() { String::new() } else { format!("{}{}", indent(l.level), l.code) };
        out_lines.push(match &l.trailing {
            Some(c) => format!("{} {}", head, c),
            None => head,
        });
        i += 1;
    }
    let _ = lines.iter().any(|l| l.raw);
    while out_lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        out_lines.pop();
    }
    let nl = if crlf { "\r\n" } else { "\n" };
    let mut s: String = out_lines.iter().map(|l| l.trim_end()).collect::<Vec<_>>().join(nl);
    s.push_str(nl);
    s
}

fn ends_statement(t: &Tok) -> bool {
    matches!(t.text.as_str(), ";" | "{" | "}")
        || matches!(t.kind.as_str(), "func_desc" | "pragma_strict" | "comment")
        || matches!(t.parent.as_str(), "include" | "appendto" | "nowarn")
        || t.text == "else"
}

/// Line breaks before token t (prev = the token before it).
fn newlines(p: &Tok, t: &Tok, _pp: Option<&Tok>, style: &Style, force: bool) -> usize {
    let orig = if t.virt { 0 } else { t.nl_before };
    let at_least = |n: usize| orig.max(n);
    if force || p.is_line_comment() {
        return at_least(1);
    }
    // an empty block stays {}
    if p.opens_block() && t.role == Role::BlockClose {
        return 0;
    }
    // a short function stays on one line: func F() { return 1; }
    if p.inline && p.opens_block() || t.inline && t.role == Role::BlockClose {
        return 0;
    }
    match t.role {
        Role::FuncOpen => return if style.function_brace_next_line { 1 } else { 0 },
        Role::CtrlOpen => return if style.control_brace_next_line { 1 } else { 0 },
        Role::BlockClose => return at_least(1),
        _ => {}
    }
    if p.opens_block() {
        return at_least(1);
    }
    if p.text == "else" && t.text == "if" {
        return 0; // else if
    }
    if p.role == Role::BlockClose {
        if t.text == "else" {
            return if style.else_on_new_line { 1 } else { 0 };
        }
        if matches!(t.text.as_str(), ";" | "," | ")") {
            return 0;
        }
        return at_least(1);
    }
    orig
}

/// Block comment text after the comment rules (lines joined with '\n', continuation lines start with the
/// indentation marker '\u{1}' replaced by the caller... here: plain " * " prefixes, indented by the line level).
fn comment_text(text: &str, style: &Style) -> String {
    let t = text.trim();
    if !t.ends_with("*/") || t.len() < 4 {
        return text.to_string(); // unterminated /* at the end of the script (the engine accepts it): untouched
    }
    if !t.contains('\n') {
        if style.header_comment {
            if let Some(inner) = t.strip_prefix("/*").and_then(|x| x.strip_suffix("*/")) {
                let inner = inner.trim();
                if inner.starts_with("--") && inner.ends_with("--") && inner.len() >= 4 {
                    let name = inner.trim_matches('-').trim();
                    return format!("/*-- {} --*/", name);
                }
            }
        }
        return t.to_string();
    }
    if !style.uniform_block_comments {
        return t.to_string();
    }
    let body = t.strip_prefix("/*").and_then(|x| x.strip_suffix("*/")).unwrap_or(t);
    let mut lines: Vec<&str> = body.lines().collect();
    let first = lines.remove(0).trim();
    while lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        lines.pop();
    }
    let mut out = String::from("/*");
    if !first.is_empty() && !first.chars().all(|c| c == '*') {
        out.push_str("\n * ");
        out.push_str(first.trim_start_matches('*').trim());
    } else if first.chars().all(|c| c == '*') && !first.is_empty() {
        out.push_str(first); // /*** banner: keep its stars
    }
    for l in lines {
        let l = l.trim();
        let l = l.strip_prefix('*').map(|x| x.strip_prefix(' ').unwrap_or(x)).unwrap_or(l);
        out.push_str("\n *");
        if !l.is_empty() {
            out.push(' ');
            out.push_str(l.trim_end());
        }
    }
    out.push_str("\n */");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(src: &str) -> String {
        format(src, &Style::default()).expect("formatted")
    }

    #[test]
    fn braces_spaces_and_indent() {
        let src = "#strict 3\nfunc f(int a,int b)\n{\nif(a>b) return a;\nelse\nreturn b;\n}\n";
        assert_eq!(fmt(src), "#strict 3\nfunc f(int a, int b) {\n  if (a > b) {\n    return a;\n  } else {\n    return b;\n  }\n}\n");
    }

    #[test]
    fn next_line_braces() {
        let st = Style { function_brace_next_line: true, ..Style::default() };
        let src = "#strict 3\nfunc f() { while (true) { break; } }\n";
        assert_eq!(format(src, &st).unwrap(), "#strict 3\nfunc f()\n{\n  while (true) {\n    break;\n  }\n}\n");
    }

    #[test]
    fn trailing_comments_aligned_and_header() {
        let src = "#strict 3\n/* -- Demo -- */\nfunc f() {\n  var x = 5; // speed\n  var name = \"abc\"; // label\n}\n";
        assert_eq!(fmt(src), "#strict 3\n/*-- Demo --*/\nfunc f() {\n  var x = 5;        // speed\n  var name = \"abc\"; // label\n}\n");
    }

    #[test]
    fn block_comment_uniform() {
        let src = "#strict 3\n/* Explodes.\n   Radius in px. */\nfunc f() {}\n";
        assert_eq!(fmt(src), "#strict 3\n/*\n * Explodes.\n * Radius in px.\n */\nfunc f() {}\n");
    }

    #[test]
    fn calls_unary_arrows_maps_and_for() {
        let src = "#strict 3\nfunc f(object o) {\n  var m = {speed=5, name=\"x\"};\n  for(var i=0;i<3;i++) o->~Foo(-i, !m?.speed);\n  return m.speed ?? 0;\n}\n";
        assert_eq!(fmt(src), "#strict 3\nfunc f(object o) {\n  var m = {speed = 5, name = \"x\"};\n  for (var i = 0; i < 3; i++) {\n    o->~Foo(-i, !m?.speed);\n  }\n  return m.speed ?? 0;\n}\n");
    }

    #[test]
    fn idempotent_and_keeps_old_scripts() {
        let src = "#strict 3\nfunc f(int a,int b)\n{\nif(a>b) return a; // big\nelse\nreturn b;\n}\n";
        let once = fmt(src);
        assert_eq!(fmt(&once), once);
        assert!(format("func f() { return(1); }\nOld:\n  return(1);\n", &Style::default()).is_none());
    }

    #[test]
    fn else_if_on_one_line_and_structure_kept() {
        let src = "#strict 2
func f() {
  if (a) {
    X();
  }
  else
    if (b) Y();
}
";
        assert_eq!(fmt(src), "#strict 2
func f() {
  if (a) {
    X();
  } else if (b) {
    Y();
  }
}
");
        // dangling else stays with the inner if
        let src = "#strict 2
func f() {
  if (a)
    if (b) X();
    else Y();
}
";
        assert_eq!(fmt(src), "#strict 2
func f() {
  if (a) {
    if (b) {
      X();
    } else {
      Y();
    }
  }
}
");
    }

    #[test]
    fn structure_check_sees_a_moved_else() {
        let a = "#strict 2
func f() { if (a) if (b) X(); else Y(); }
";
        let b = "#strict 2
func f() { if (a) { if (b) X(); } else Y(); }
";
        let c = "#strict 2
func f() { if (a) { if (b) { X(); } else { Y(); } } }
";
        let sh = |s: &str| shape(&new_parser().parse(s, None).unwrap().root_node(), s);
        assert_ne!(sh(a), sh(b), "the else moved to the outer if");
        assert_eq!(sh(a), sh(c), "only braces added");
    }

    #[test]
    fn style_file() {
        let s = Style::parse("# team rules\nfunction_brace: next_line\nindent_width: 4\n").unwrap();
        assert!(s.function_brace_next_line && s.indent_width == 4 && !s.control_brace_next_line);
        assert!(Style::parse("funtion_brace: next_line").is_err());
    }

    #[test]
    fn short_functions_stay_on_one_line() {
        let src = "#strict 3\npublic func IsDeployed() { return deployed; }\nfunc F() { Foo(1); }\nfunc G() { if (x) return 1; }\n";
        assert_eq!(fmt(src), "#strict 3\npublic func IsDeployed() { return deployed; }\nfunc F() { Foo(1); }\nfunc G() {\n  if (x) {\n    return 1;\n  }\n}\n");
        let st = Style { short_functions: false, ..Style::default() };
        assert_eq!(format("#strict 3\nfunc F() { Foo(1); }\n", &st).unwrap(), "#strict 3\nfunc F() {\n  Foo(1);\n}\n");
    }
}
