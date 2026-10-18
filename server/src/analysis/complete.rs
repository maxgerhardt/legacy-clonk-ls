//! Autocomplete and signature help for C4Script.
//!
//! The context comes from a small lexer over the text before the cursor (the syntax tree is often broken while
//! typing): after `->` the functions of the receiver's definition (known from `this`, an ID, `CreateObject(ID..)`,
//! `FindObject(Find_ID(ID))`, ... or a variable assigned from one; else every reachable script function), after
//! `m.` the keys of map literals assigned to `m` (or the locals of an object's definition), after `#` the
//! directives, after `#include`/`#appendto` the IDs, inside a string after `$` the string table entries, and
//! elsewhere parameters/variables of the current function, script variables, visible and engine functions,
//! engine constants, IDs and keywords.

use super::engine::{ENGINE, KEYWORDS};
use super::index::{path_key, FileInfo, FuncDef, Index, VarKind};
use super::text::LineIndex;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, Documentation, InsertTextFormat, MarkupContent, MarkupKind, ParameterInformation,
    ParameterLabel, Position, SignatureHelp, SignatureInformation,
};

/// The client accepts snippets (`Name($0)`): set from the initialize capabilities.
pub static SNIPPETS: AtomicBool = AtomicBool::new(false);

const TYPES: &[&str] = &["int", "bool", "id", "object", "string", "array", "map", "any"];

#[derive(Debug, Clone, Copy, PartialEq)]
enum K {
    Word,
    Str,
    Punct,
}

#[derive(Debug, Clone, Copy)]
struct Tok<'a> {
    text: &'a str,
    kind: K,
}

impl<'a> Tok<'a> {
    fn is(&self, s: &str) -> bool {
        self.kind != K::Str && self.text == s
    }
    fn word(&self) -> Option<&'a str> {
        (self.kind == K::Word).then_some(self.text)
    }
}

/// Tokens of C4Script text (comments skipped, strings as one token, operators of two chars joined).
fn lex(src: &str) -> Vec<Tok<'_>> {
    let b = src.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if b[i..].starts_with(b"//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i..].starts_with(b"/*") {
            i = src[i + 2..].find("*/").map(|e| i + 2 + e + 2).unwrap_or(b.len());
        } else if c == b'"' {
            let s = i;
            i += 1;
            while i < b.len() && b[i] != b'"' && b[i] != b'\n' {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i = (i + 1).min(b.len());
            out.push(Tok { text: &src[s..i], kind: K::Str });
        } else if c.is_ascii_alphanumeric() || c == b'_' {
            let s = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            out.push(Tok { text: &src[s..i], kind: K::Word });
        } else {
            let two = src.get(i..i + 2).unwrap_or("");
            let n = if ["->", "==", "!=", "<=", ">=", "&&", "||", "??", "..", "::", "+=", "-=", "*=", "/="].contains(&two) { 2 } else { 1 };
            let n = if c >= 0x80 { src[i..].chars().next().map(|ch| ch.len_utf8()).unwrap_or(1) } else { n };
            out.push(Tok { text: &src[i..i + n], kind: K::Punct });
            i += n;
        }
    }
    out
}

/// Index of the `(`/`[`/`{` matching the closer at `close`.
fn matching_open(toks: &[Tok], close: usize) -> Option<usize> {
    let mut depth = 0;
    for j in (0..=close).rev() {
        match toks[j].text {
            ")" | "]" | "}" if toks[j].kind == K::Punct => depth += 1,
            "(" | "[" | "{" if toks[j].kind == K::Punct => {
                depth -= 1;
                if depth == 0 {
                    return Some(j);
                }
            }
            _ => {}
        }
    }
    None
}

/// The function the cursor is in: (start of its tokens, its parameters as (type, name)), from the last `func`
/// whose body is still open at the end of `toks`.
fn current_function<'a>(toks: &[Tok<'a>]) -> Option<(usize, Vec<(Option<&'a str>, &'a str)>)> {
    let f = toks.iter().rposition(|t| t.is("func"))?;
    let open = (f..toks.len()).find(|&j| toks[j].is("("))?;
    let close = (open..toks.len()).find(|&j| toks[j].is(")"))?;
    let mut depth = 0;
    let mut opened = false;
    for t in &toks[close..] {
        if t.is("{") {
            depth += 1;
            opened = true;
        } else if t.is("}") {
            depth -= 1;
            if opened && depth == 0 {
                return None; // that function ended before the cursor
            }
        }
    }
    let mut params = vec![];
    for p in toks[open + 1..close].split(|t| t.is(",")) {
        let words: Vec<&str> = p.iter().filter_map(|t| t.word()).collect();
        match words.as_slice() {
            [name] => params.push((None, *name)),
            [.., ty, name] => params.push((Some(*ty), *name)),
            _ => {}
        }
    }
    Some((f, params))
}

/// `var` declarations of a function body: `var a`, `var a = 1, b`, `for (var x in arr)`.
fn declared_vars<'a>(toks: &[Tok<'a>]) -> Vec<&'a str> {
    let mut out = vec![];
    for (i, t) in toks.iter().enumerate() {
        if !t.is("var") {
            continue;
        }
        let mut j = i + 1;
        let mut depth = 0;
        let mut expect_name = true;
        while j < toks.len() {
            let u = toks[j];
            if expect_name {
                match u.word() {
                    Some(w) if depth == 0 => out.push(w),
                    _ => break,
                }
                expect_name = false;
            } else if u.is("(") || u.is("[") || u.is("{") {
                depth += 1;
            } else if u.is(")") || u.is("]") || u.is("}") {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            } else if depth == 0 && u.is(",") {
                expect_name = true;
            } else if depth == 0 && (u.is(";") || u.is("in")) {
                break;
            }
            j += 1;
        }
    }
    out
}

fn id_like(s: &str) -> bool {
    s.len() == 4 && s.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_') && s.bytes().any(|c| c.is_ascii_uppercase())
}

fn known_id(index: &Index, info: &FileInfo, s: &str) -> bool {
    id_like(s) && (index.id_for(s, &info.path).is_some() || super::diagnostics::standard_id(s).is_some())
}

struct Ctx<'a> {
    index: &'a Index,
    info: &'a FileInfo,
    /// tokens before the cursor (without the word being typed)
    toks: Vec<Tok<'a>>,
    /// where the current function's tokens start
    func: Option<usize>,
}

impl<'a> Ctx<'a> {
    /// Definition ID of a call result: Create*/Find*/Place*/Compose* calls with an ID argument.
    fn call_def(&self, name: &str, args: &[Tok]) -> Option<String> {
        if !["Create", "Find", "Place", "Compose", "Buy"].iter().any(|p| name.starts_with(p)) {
            return None;
        }
        args.iter().filter_map(|t| t.word()).find(|w| known_id(self.index, self.info, w)).map(|w| w.to_string())
    }

    /// Definition ID of the expression ending before token `end` (exclusive), e.g. the receiver of `->`.
    fn expr_def(&self, end: usize, depth: u8) -> Option<String> {
        let last = end.checked_sub(1)?;
        let t = self.toks[last];
        if t.is(")") {
            let open = matching_open(&self.toks, last)?;
            let name = self.toks.get(open.checked_sub(1)?)?.word()?;
            return self.call_def(name, &self.toks[open + 1..last]);
        }
        let w = t.word()?;
        if w == "this" {
            return self.info.def_id.clone();
        }
        if known_id(self.index, self.info, w) {
            return Some(w.to_string());
        }
        if depth > 2 {
            return None;
        }
        // a variable: its last assignment before (in the current function)
        let from = self.func.unwrap_or(0);
        let toks = &self.toks[from..last];
        let at = (0..toks.len().saturating_sub(1)).rev().find(|&j| toks[j].is(w) && toks[j + 1].is("="))?;
        let rhs_start = from + at + 2;
        let mut j = rhs_start;
        let mut d = 0;
        while j < last {
            let u = self.toks[j];
            if u.is("(") || u.is("[") || u.is("{") {
                d += 1;
            } else if u.is(")") || u.is("]") || u.is("}") {
                if d == 0 {
                    break;
                }
                d -= 1;
            } else if d == 0 && (u.is(";") || u.is(",")) {
                break;
            }
            j += 1;
        }
        if j <= rhs_start {
            return None;
        }
        self.expr_def(j, depth + 1)
    }

    /// The script of a definition and the scripts it sees (#include chain, #appendto).
    fn def_scope(&self, id: &str) -> Vec<&'a FileInfo> {
        if self.info.def_id.as_deref() == Some(id) {
            return self.index.scope_files(self.info);
        }
        let script = self.index.id_for(id, &self.info.path).and_then(|i| i.script.as_ref());
        match script.and_then(|s| self.index.files.get(&path_key(s))) {
            Some(f) => self.index.scope_files(f),
            None => vec![],
        }
    }
}

struct Items {
    out: Vec<CompletionItem>,
    seen: HashSet<String>,
    /// the text right after the cursor starts with '(': don't insert another pair
    paren_follows: bool,
}

impl Items {
    fn add(&mut self, label: &str, kind: CompletionItemKind, detail: String, doc: Option<String>, sort: u8) {
        if !self.seen.insert(label.to_string()) {
            return;
        }
        self.out.push(CompletionItem {
            label: label.to_string(),
            kind: Some(kind),
            detail: (!detail.is_empty()).then_some(detail),
            documentation: doc.map(|d| Documentation::MarkupContent(MarkupContent { kind: MarkupKind::Markdown, value: d })),
            sort_text: Some(format!("{}{}", sort, label.to_lowercase())),
            ..Default::default()
        });
    }

    fn add_func(&mut self, name: &str, kind: CompletionItemKind, detail: String, doc: Option<String>, sort: u8, has_params: bool) {
        let before = self.out.len();
        self.add(name, kind, detail, doc, sort);
        if self.out.len() == before || self.paren_follows {
            return;
        }
        let item = self.out.last_mut().unwrap();
        if SNIPPETS.load(Ordering::Relaxed) {
            item.insert_text = Some(if has_params { format!("{}($0)", name) } else { format!("{}()", name) });
            item.insert_text_format = Some(InsertTextFormat::SNIPPET);
            if has_params {
                // open the parameter hints right away
                item.command = Some(tower_lsp::lsp_types::Command {
                    title: "Signature help".to_string(),
                    command: "editor.action.triggerParameterHints".to_string(),
                    arguments: None,
                });
            }
        }
    }

    fn script_func(&mut self, index: &Index, f: &FileInfo, d: &FuncDef, kind: CompletionItemKind, sort: u8, docs: bool) {
        let owner = f.def_id.clone().unwrap_or_else(|| "script".to_string());
        let doc = docs.then(|| super::query::func_markdown(f, d, index));
        self.add_func(&d.name, kind, format!("{}  ({})", d.signature(), owner), doc, sort, !d.params.is_empty());
    }

    fn engine_funcs(&mut self, kind: CompletionItemKind, sort: u8) {
        let mut names: Vec<&String> = ENGINE.functions.keys().collect();
        names.sort();
        for n in names {
            let f = &ENGINE.functions[n];
            let has_params = f.params.as_ref().map(|p| !p.is_empty()).unwrap_or(true);
            self.add_func(n, kind, f.signature(n), Some(f.hover_markdown(n)), sort, has_params);
        }
    }
}

/// Completion items at `pos`; None when nothing fits (e.g. `a >` typed, or inside a comment).
pub fn complete(index: &Index, info: &FileInfo, src: &str, pos: Position, trigger: Option<&str>) -> Option<Vec<CompletionItem>> {
    let li = LineIndex::new(src);
    let at = li.to_byte(pos).min(src.len());
    let before = &src[..at];
    let word_len = before.bytes().rev().take_while(|c| c.is_ascii_alphanumeric() || *c == b'_').count();
    let start = at - word_len;
    let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line = &src[line_start..start];
    let mut items = Items { out: vec![], seen: HashSet::new(), paren_follows: src[at..].trim_start().starts_with('(') };

    // inside a comment or string?
    let toks_line = lex(line);
    if in_block_comment(&src[..start]) {
        return None;
    }
    if let Some(last) = toks_line.last().filter(|t| t.kind == K::Str && (t.text.len() < 2 || !t.text.ends_with('"'))) {
        // an open string: $Name$ string table entries after an unclosed '$'
        if last.text.matches('$').count() % 2 == 1 {
            let close = !src[at..].starts_with('$');
            for t in super::strings::tables_for(&info.path) {
                let mut keys: Vec<_> = t.entries.iter().collect();
                keys.sort_by_key(|(k, _)| k.to_string());
                for (k, (text, _)) in keys {
                    let before_len = items.out.len();
                    items.add(k, CompletionItemKind::TEXT, text.clone(), Some(format!("`StringTbl{}.txt`: {}", t.lang, text)), 0);
                    if close && items.out.len() > before_len {
                        items.out.last_mut().unwrap().insert_text = Some(format!("{}$", k));
                    }
                }
            }
            return Some(items.out);
        }
        return None;
    }

    // directives
    let trimmed = line.trim_start();
    if trimmed == "#" {
        for (d, doc) in [("strict 3", "#strict 3: nil, maps, obj.key, ?? and ?-> (what new scripts use)"), ("strict 2", ""), ("include", "#include ID: inherit the script of a definition"), ("appendto", "#appendto ID: add functions to a definition's script")] {
            items.add(d, CompletionItemKind::KEYWORD, doc.to_string(), None, 0);
        }
        return Some(items.out);
    }
    if trimmed.starts_with("#include") || trimmed.starts_with("#appendto") {
        if trimmed.starts_with("#appendto") {
            items.add("*", CompletionItemKind::KEYWORD, "every definition".to_string(), None, 0);
        }
        add_ids(&mut items, index, info, 0);
        return Some(items.out);
    }

    let toks = lex(&src[..start]);
    let func = current_function(&toks).map(|(f, _)| f);
    let ctx = Ctx { index, info, toks: toks.clone(), func };
    let n = toks.len();
    let last = toks.last().copied();

    // obj->Member / obj->~Member
    let arrow_at = match (n.checked_sub(1).map(|i| toks[i]), n.checked_sub(2).map(|i| toks[i])) {
        (Some(t), _) if t.is("->") => Some(n - 1),
        (Some(t), Some(u)) if t.is("~") && u.is("->") => Some(n - 2),
        _ => None,
    };
    if let Some(a) = arrow_at {
        let recv_global = a > 0 && toks[a - 1].is("global");
        if recv_global {
            for f in index.files.values().filter(|f| index.visible_from(&info.path, &f.path)) {
                for d in f.funcs.iter().filter(|d| d.visibility.as_deref() == Some("global")) {
                    items.script_func(index, f, d, CompletionItemKind::FUNCTION, 0, true);
                }
            }
            items.engine_funcs(CompletionItemKind::FUNCTION, 1);
            return Some(items.out);
        }
        match ctx.expr_def(a, 0) {
            Some(id) => {
                for f in ctx.def_scope(&id) {
                    for d in &f.funcs {
                        if d.visibility.as_deref() != Some("global") {
                            items.script_func(index, f, d, CompletionItemKind::METHOD, 0, true);
                        }
                    }
                }
            }
            None => {
                // unknown object: every script function this script can reach (no docs: thousands of items)
                let mut files: Vec<&FileInfo> = std::iter::once(info)
                    .chain(index.files.values().filter(|f| path_key(&f.path) != path_key(&info.path) && index.visible_from(&info.path, &f.path)))
                    .collect();
                files.sort_by_key(|f| path_key(&f.path));
                for f in files {
                    for d in f.funcs.iter().filter(|d| d.visibility.as_deref() != Some("global")) {
                        items.script_func(index, f, d, CompletionItemKind::METHOD, 0, false);
                    }
                }
            }
        }
        items.engine_funcs(CompletionItemKind::FUNCTION, 1);
        return Some(items.out);
    }

    // m.key: keys of map literals assigned to m (and m.key = ...), or the locals of an object's definition
    if let Some(t) = last.filter(|t| t.is(".")) {
        let _ = t;
        let recv = n.checked_sub(2).and_then(|i| toks[i].word());
        if let Some(name) = recv.filter(|w| !w.bytes().all(|c| c.is_ascii_digit())) {
            let from = func.unwrap_or(0);
            let body = &toks[from..n - 1];
            for j in 0..body.len() {
                // name = { key = ..., key2 = ... }
                if body[j].is(name) && body.get(j + 1).map_or(false, |u| u.is("=")) && body.get(j + 2).map_or(false, |u| u.is("{")) {
                    let mut depth = 0;
                    let mut k = j + 2;
                    while k < body.len() {
                        let u = body[k];
                        if u.is("{") || u.is("(") || u.is("[") {
                            depth += 1;
                        } else if u.is("}") || u.is(")") || u.is("]") {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        } else if depth == 1 && (body[k - 1].is("{") || body[k - 1].is(",")) && body.get(k + 1).map_or(false, |v| v.is("=")) {
                            if let Some(key) = u.word() {
                                items.add(key, CompletionItemKind::PROPERTY, format!("key of {}", name), None, 0);
                            }
                        }
                        k += 1;
                    }
                }
                // name.key = ...
                if body[j].is(name) && body.get(j + 1).map_or(false, |u| u.is(".")) {
                    if let Some(key) = body.get(j + 2).and_then(|u| u.word()) {
                        items.add(key, CompletionItemKind::PROPERTY, format!("key of {}", name), None, 0);
                    }
                }
            }
            if let Some(id) = ctx.expr_def(n - 1, 0) {
                for f in ctx.def_scope(&id) {
                    for v in f.vars.iter().filter(|v| v.kind == VarKind::Local) {
                        items.add(&v.name, CompletionItemKind::FIELD, format!("local of {}", f.def_id.as_deref().unwrap_or(&id)), None, 1);
                    }
                }
            }
            return Some(items.out);
        }
        return None;
    }
    if trigger.is_some() && word_len == 0 {
        return None; // '>' of a comparison, '..', '$' outside a string, ...
    }
    // a name being declared: func Name, var name, local name
    if last.map_or(false, |t| t.is("func") || t.is("var") || t.is("local") || t.is("static") || t.is("const")) {
        return None;
    }

    // everything visible here
    if let Some(f) = func {
        if let Some((_, params)) = current_function(&toks) {
            for (ty, p) in params {
                items.add(p, CompletionItemKind::VARIABLE, format!("parameter{}", ty.map(|t| format!(" ({})", t)).unwrap_or_default()), None, 0);
            }
        }
        for v in declared_vars(&toks[f..]) {
            items.add(v, CompletionItemKind::VARIABLE, "local variable".to_string(), None, 0);
        }
    }
    let scope = index.scope_files(info);
    for f in &scope {
        for v in &f.vars {
            let (kind, what) = match v.kind {
                VarKind::Local => (CompletionItemKind::FIELD, "local"),
                VarKind::Static => (CompletionItemKind::VARIABLE, "static"),
                VarKind::StaticConst => (CompletionItemKind::CONSTANT, "static const"),
            };
            let value = v.value.as_ref().map(|x| format!(" = {}", x)).unwrap_or_default();
            items.add(&v.name, kind, format!("{} {}{}", what, v.name, value), None, 1);
        }
    }
    for f in &scope {
        for d in &f.funcs {
            items.script_func(index, f, d, CompletionItemKind::FUNCTION, 1, true);
        }
    }
    let mut others: Vec<&FileInfo> = index.files.values().filter(|f| index.visible_from(&info.path, &f.path)).collect();
    others.sort_by_key(|f| path_key(&f.path));
    for f in &others {
        for d in f.funcs.iter().filter(|d| d.visibility.as_deref() == Some("global")) {
            items.script_func(index, f, d, CompletionItemKind::FUNCTION, 2, true);
        }
        for v in f.vars.iter().filter(|v| v.kind != VarKind::Local) {
            let value = v.value.as_ref().map(|x| format!(" = {}", x)).unwrap_or_default();
            let kind = if v.kind == VarKind::StaticConst { CompletionItemKind::CONSTANT } else { CompletionItemKind::VARIABLE };
            items.add(&v.name, kind, format!("static {}{}  ({})", v.name, value, f.def_id.as_deref().unwrap_or("script")), None, 2);
        }
    }
    items.engine_funcs(CompletionItemKind::FUNCTION, 3);
    let mut consts: Vec<_> = ENGINE.constants.iter().collect();
    consts.sort_by_key(|(n, _)| n.to_string());
    for (name, c) in consts {
        items.add(name, CompletionItemKind::ENUM_MEMBER, format!("const {} {} = {}", c.ty, name, c.value), None, 4);
    }
    add_ids(&mut items, index, info, 5);
    for k in ["this", "nil", "true", "false"] {
        items.add(k, CompletionItemKind::KEYWORD, String::new(), None, 4);
    }
    for k in KEYWORDS.iter().chain(TYPES.iter()).chain(["public", "protected", "private", "const"].iter()) {
        items.add(k, CompletionItemKind::KEYWORD, String::new(), None, 6);
    }
    Some(items.out)
}

/// Inside a comment (an unterminated /* or a // on the cursor's line)?
fn in_block_comment(src: &str) -> bool {
    let b = src.as_bytes();
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        if in_str {
            if b[i] == b'\\' {
                i += 1;
            } else if b[i] == b'"' || b[i] == b'\n' {
                in_str = false;
            }
        } else if b[i] == b'"' {
            in_str = true;
        } else if b[i..].starts_with(b"//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            if i == b.len() {
                return true;
            }
        } else if b[i..].starts_with(b"/*") {
            match src[i + 2..].find("*/") {
                Some(e) => i += 2 + e + 1,
                None => return true,
            }
        }
        i += 1;
    }
    false
}

fn add_ids(items: &mut Items, index: &Index, info: &FileInfo, sort: u8) {
    let mut ids: Vec<&String> = index.ids.keys().filter(|id| index.id_for(id, &info.path).is_some()).collect();
    ids.sort();
    for id in ids {
        let name = index.id_for(id, &info.path).and_then(|i| i.name.clone()).unwrap_or_default();
        items.add(id, CompletionItemKind::CLASS, name, None, sort);
    }
    for i in crate::workspace::Workspace::new().get_ids() {
        items.add(&i.id, CompletionItemKind::CLASS, format!("{} ({})", i.name, i.tlc), None, sort);
    }
}

/// Parameter hints for the call around `pos`.
pub fn signature_help(index: &Index, info: &FileInfo, src: &str, pos: Position) -> Option<SignatureHelp> {
    let li = LineIndex::new(src);
    let at = li.to_byte(pos).min(src.len());
    let toks = lex(&src[..at]);
    // the innermost open '(' and the commas before the cursor at its level
    let mut depth = 0;
    let mut commas = 0u32;
    let mut open = None;
    for j in (0..toks.len()).rev() {
        let t = toks[j];
        if t.is(")") || t.is("]") || t.is("}") {
            depth += 1;
        } else if t.is("(") || t.is("[") {
            if depth == 0 {
                if t.is("(") {
                    open = Some(j);
                }
                break;
            }
            depth -= 1;
        } else if t.is("{") {
            if depth == 0 {
                break; // a block (or map) starts: no call around the cursor
            }
            depth -= 1;
        } else if depth == 0 && t.is(";") {
            break;
        } else if depth == 0 && t.is(",") {
            commas += 1;
        }
    }
    let open = open?;
    let name = toks.get(open.checked_sub(1)?)?.word()?;
    if ["if", "while", "for", "return", "func"].contains(&name) || open >= 2 && toks[open - 2].is("func") {
        return None;
    }
    let arrow = open >= 2 && (toks[open - 2].is("->") || toks[open - 2].is("~") && open >= 3 && toks[open - 3].is("->"));
    let mut sigs = vec![];
    let script_defs: Vec<(&FileInfo, &FuncDef)> = if arrow {
        let arrow_at = if toks[open - 2].is("->") { open - 2 } else { open - 3 };
        let ctx = Ctx { index, info, toks: toks.clone(), func: current_function(&toks).map(|(f, _)| f) };
        match ctx.expr_def(arrow_at, 0) {
            Some(id) => ctx.def_scope(&id).into_iter().flat_map(|f| f.funcs.iter().filter(|d| d.name == name).map(move |d| (f, d))).collect(),
            None => index.reachable_funcs(&info.path, name),
        }
    } else {
        index.visible_funcs(info, name)
    };
    for (f, d) in script_defs.iter().take(5) {
        let mut label = format!("func {}(", d.name);
        let mut params = vec![];
        for (i, (ty, p)) in d.params.iter().enumerate() {
            if i > 0 {
                label.push_str(", ");
            }
            let s = label.len() as u32;
            label.push_str(&match ty {
                Some(t) => format!("{} {}", t, p),
                None => p.to_string(),
            });
            params.push(ParameterInformation { label: ParameterLabel::LabelOffsets([s, label.len() as u32]), documentation: None });
        }
        label.push(')');
        sigs.push(SignatureInformation {
            label,
            documentation: Some(Documentation::MarkupContent(MarkupContent { kind: MarkupKind::Markdown, value: super::query::func_markdown(f, d, index) })),
            parameters: Some(params),
            active_parameter: None,
        });
    }
    if sigs.is_empty() || arrow {
        if let Some(ef) = ENGINE.functions.get(name) {
            let rtype = if ef.rtype.is_empty() { "any" } else { &ef.rtype };
            let mut label = format!("{} {}(", rtype, name);
            let mut params = vec![];
            for (i, p) in ef.params.iter().flatten().enumerate() {
                if i > 0 {
                    label.push_str(", ");
                }
                let s = label.len() as u32;
                label.push_str(&format!("{} {}", p.ty, p.name));
                let doc = (!p.desc.is_empty()).then(|| Documentation::String(p.desc.clone()));
                params.push(ParameterInformation { label: ParameterLabel::LabelOffsets([s, label.len() as u32]), documentation: doc });
            }
            label.push(')');
            let doc = (!ef.desc.is_empty()).then(|| Documentation::MarkupContent(MarkupContent { kind: MarkupKind::Markdown, value: ef.desc.clone() }));
            sigs.push(SignatureInformation { label, documentation: doc, parameters: Some(params), active_parameter: None });
        }
    }
    if sigs.is_empty() {
        return None;
    }
    Some(SignatureHelp { signatures: sigs, active_signature: Some(0), active_parameter: Some(commas) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_context() {
        let src = "#strict 3\nlocal x;\nfunc Done() { var gone; }\nfunc F(int a, object b) {\n  var c = 1, d;\n  for (var e in arr) {\n    ";
        let toks = lex(src);
        let (f, params) = current_function(&toks).unwrap();
        assert_eq!(params, vec![(Some("int"), "a"), (Some("object"), "b")]);
        assert_eq!(declared_vars(&toks[f..]), vec!["c", "d", "e"]);
        // after the function's closing brace there is no current function
        assert!(current_function(&lex("func F() { var a; }\n")).is_none());
    }

    #[test]
    fn lexer_skips_comments_and_strings() {
        let toks = lex("a->B(\"x->y\") // c->d\n/* e->f */ g");
        let texts: Vec<&str> = toks.iter().map(|t| t.text).collect();
        assert_eq!(texts, vec!["a", "->", "B", "(", "\"x->y\"", ")", "g"]);
        assert!(in_block_comment("x /* open"));
        assert!(!in_block_comment("x /* closed */ y \"/*\""));
        assert!(in_block_comment("x // a line comment") && !in_block_comment("x // done\ny") && !in_block_comment("\"//\" x"));
    }

    #[test]
    fn ids_look_like_ids() {
        assert!(id_like("FLNT") && id_like("_AB1") && !id_like("1234") && !id_like("Flnt") && !id_like("FLNTS"));
    }
}
