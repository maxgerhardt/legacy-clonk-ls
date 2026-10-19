//! Lint checks the engine doesn't make (unused variables and functions, unreachable code) and suppression comments.
//!
//! Suppression (clang-tidy style, also our own spelling), in line or block comments, optionally with codes:
//! `// NOLINT`, `// NOLINT(nil-call, unused-var)`: this line; `// NOLINTNEXTLINE(...)`: the next line;
//! `// c4lint: ignore(...)`, `// c4lint: ignore-next-line(...)`, `// c4lint: ignore-file(...)`. No codes = all.

use super::index::{children, named_children, path_key, FileInfo, Index};
use super::text::{node_text, LineIndex};
use std::collections::{HashMap, HashSet};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, DiagnosticTag, NumberOrString};
use tree_sitter::Node;

/// Every diagnostic code with a short description (completion inside `// NOLINT(`).
pub const CODES: &[(&str, &str)] = &[
    ("syntax", "syntax error"),
    ("strict", "#strict level: syntax the level doesn't allow"),
    ("no-strict", "no #strict directive"),
    ("unknown-identifier", "unknown function or variable"),
    ("unknown-id", "unknown definition ID"),
    ("unknown-method", "obj->F(): no indexed script defines F"),
    ("direct-call", "obj->F() without ~: no loaded script or engine function F"),
    ("member-call", "obj->F(): the object's definition has no F"),
    ("too-many-params", "more parameters than the function takes"),
    ("arg-type", "an argument the engine can't convert"),
    ("op-type", "an operand the engine can't use (nil in #strict 3, never an int)"),
    ("strict3-nothing", "0/false passed for an object/string/array in #strict 3"),
    ("nil-call", "obj->F() where obj may be nil (nilChecks)"),
    ("inherited", "inherited() without an overloaded function"),
    ("id-label", "a name that looks like an ID before ( or :"),
    ("local-in-global", "a local used in a global func"),
    ("string-table", "$Name$ not in the string table"),
    ("func-desc", "a [..|Image=..|Condition=..] function description the engine reads differently"),
    ("action-name", "an action name the object's ActMap doesn't have (SetAction/GetAction)"),
    ("sound-name", "a sound name no sound file matches"),
    ("graphics-name", "a SetGraphics name without a Graphics<Name>.png/.bmp"),
    ("ini-section", "DefCore/ActMap/Scenario.txt: a section the engine doesn't read"),
    ("ini-key", "DefCore/ActMap/Scenario.txt: a key the engine doesn't read (keys are case-sensitive)"),
    ("ini-value", "DefCore/ActMap/Scenario.txt: a value of the wrong kind, or a facet/picture outside the graphics"),
    ("ini-id", "a definition ID another definition of the same pack/scenario uses too"),
    ("def-graphics", "a definition without Graphics.png/.bmp (the engine skips it)"),
    ("unused-var", "a var that is never read"),
    ("unused-param", "a parameter of a private function that is never used"),
    ("unused-local", "a local/static no script reads"),
    ("unused-func", "a private function nothing calls"),
    ("unreachable", "code that never runs"),
];

/// Codes suppressed per line (None = all codes), and for the whole file.
#[derive(Default)]
pub struct Suppressions {
    lines: HashMap<u32, Option<HashSet<String>>>,
    file: Option<Option<HashSet<String>>>,
}

fn merge(a: &mut Option<HashSet<String>>, b: Option<HashSet<String>>) {
    match (a.as_mut(), b) {
        (Some(x), Some(y)) => x.extend(y),
        _ => *a = None,
    }
}

/// `(a, b)` after a marker: the codes; nothing: all.
fn codes(rest: &str) -> Option<HashSet<String>> {
    let rest = rest.trim_start();
    let inner = rest.strip_prefix('(')?.split(')').next()?;
    let set: HashSet<String> = inner.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect();
    (!set.is_empty() && !set.contains("all")).then_some(set)
}

pub fn suppressions(root: &Node, src: &str) -> Suppressions {
    let mut out = Suppressions::default();
    let mut stack = vec![*root];
    while let Some(n) = stack.pop() {
        if n.kind() == "comment" {
            let text = node_text(&n, src);
            let (start, end) = (n.start_position().row as u32, n.end_position().row as u32);
            let mut add = |line: u32, c: Option<HashSet<String>>| match out.lines.get_mut(&line) {
                Some(e) => merge(e, c),
                None => {
                    out.lines.insert(line, c);
                }
            };
            if let Some(i) = text.find("NOLINTNEXTLINE") {
                add(end + 1, codes(&text[i + 14..]));
            } else if let Some(i) = text.find("NOLINT") {
                let c = codes(&text[i + 6..]);
                add(start, c.clone());
                add(end, c);
            }
            if let Some(i) = text.find("c4lint:") {
                let rest = text[i + 7..].trim_start();
                if let Some(r) = rest.strip_prefix("ignore-next-line") {
                    add(end + 1, codes(r));
                } else if let Some(r) = rest.strip_prefix("ignore-file") {
                    match out.file.as_mut() {
                        Some(e) => merge(e, codes(r)),
                        None => out.file = Some(codes(r)),
                    }
                } else if let Some(r) = rest.strip_prefix("ignore") {
                    let c = codes(r);
                    add(start, c.clone());
                    add(end, c);
                }
            }
        }
        stack.extend(named_children(&n));
    }
    out
}

impl Suppressions {
    pub fn hides(&self, d: &Diagnostic) -> bool {
        let code = match &d.code {
            Some(NumberOrString::String(s)) => s.as_str(),
            _ => "",
        };
        let covers = |c: &Option<HashSet<String>>| c.as_ref().map(|s| s.contains(code)).unwrap_or(true);
        self.file.as_ref().map(covers).unwrap_or(false) || self.lines.get(&d.range.start.line).map(covers).unwrap_or(false)
    }
}

fn diag(range: tower_lsp::lsp_types::Range, sev: DiagnosticSeverity, code: &str, msg: String) -> Diagnostic {
    Diagnostic {
        range,
        severity: Some(sev),
        code: Some(NumberOrString::String(code.to_string())),
        source: Some("c4script".to_string()),
        message: msg,
        // shown faded (unused/unreachable)
        tags: Some(vec![DiagnosticTag::UNNECESSARY]),
        ..Default::default()
    }
}

fn op(n: &Node) -> String {
    children(n).iter().find(|c| !c.is_named() && !matches!(c.kind(), "(" | ")")).map(|c| c.kind().to_string()).unwrap_or_default()
}

/// unused-var: a `var` of a function that is never read (plain `x = ...` assignments aren't reads).
pub fn unused_vars(root: &Node, src: &str, li: &LineIndex, out: &mut Vec<Diagnostic>) {
    let mut stack = vec![*root];
    while let Some(n) = stack.pop() {
        if n.kind() == "function_definition" {
            check_function_vars(&n, src, li, out);
            continue;
        }
        stack.extend(named_children(&n));
    }
}

fn check_function_vars(func: &Node, src: &str, li: &LineIndex, out: &mut Vec<Diagnostic>) {
    // declarations (first one per name) and every identifier use
    let mut decls: Vec<(String, Node)> = vec![];
    let mut loop_vars: HashSet<String> = HashSet::new();
    let mut reads: HashSet<String> = HashSet::new();
    let mut assigned: HashSet<String> = HashSet::new();
    let mut by_index = false;
    let mut stack = vec![*func];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "var_assignment" => {
                let is_var = n.parent().map(|p| named_children(&p).iter().any(|c| c.kind() == "var_scope" && node_text(c, src) == "var")).unwrap_or(false);
                let nc = named_children(&n);
                if let (true, Some(id)) = (is_var, nc.first()) {
                    let name = node_text(id, src).to_string();
                    if !decls.iter().any(|(d, _)| *d == name) {
                        decls.push((name, *id));
                    }
                    // the initial value is read as usual
                    stack.extend(nc.into_iter().skip(1));
                    continue;
                }
            }
            "for_statement" => {
                for v in super::index::for_in_vars(&n) {
                    loop_vars.insert(node_text(&v, src).to_string());
                }
            }
            // a parameter's declaration is no read
            "param" => continue,
            "method_call" if n.child_by_field_name("name").map(|x| node_text(&x, src) == "Var").unwrap_or(false) => by_index = true,
            "binary_expression" if op(&n) == "=" => {
                let nc: Vec<Node> = named_children(&n).into_iter().filter(|c| c.kind() != "comment").collect();
                if let Some(l) = nc.first().filter(|l| l.kind() == "identifier") {
                    assigned.insert(node_text(l, src).to_string());
                    stack.extend(nc.into_iter().skip(1));
                    continue;
                }
            }
            "identifier" => {
                reads.insert(node_text(&n, src).to_string());
            }
            _ => {}
        }
        stack.extend(named_children(&n));
    }
    if by_index {
        return; // Var(n) reads variables by their index
    }
    // unused-param (info): only private functions (their callers are all scripts we see; engine callbacks and
    // public functions have fixed signatures), not with Par(n) (reads parameters by index)
    let private = func.child_by_field_name("visibility").map(|v| node_text(&v, src) == "private").unwrap_or(false);
    let body = node_text(func, src);
    let callback = func.child_by_field_name("name").map(|x| super::callers::is_callback(node_text(&x, src))).unwrap_or(false);
    if private && !callback && !body.contains("Par(") {
        if let Some(pl) = named_children(func).into_iter().find(|c| c.kind() == "parameter_list") {
            for p in named_children(&pl).into_iter().filter(|c| c.kind() == "param") {
                let Some(pn) = p.child_by_field_name("name") else { continue };
                let name = node_text(&pn, src);
                if name.starts_with('_') || reads.contains(name) || assigned.contains(name) {
                    continue;
                }
                out.push(diag(li.node_range(&pn), DiagnosticSeverity::INFORMATION, "unused-param",
                    format!("parameter \"{}\" is never used (private function: remove it here and in the calls, or name it _{})", name, name)));
            }
        }
    }
    for (name, id) in decls {
        if name.starts_with('_') || loop_vars.contains(&name) || reads.contains(&name) {
            continue;
        }
        let what = if assigned.contains(&name) { "is assigned but never read" } else { "is never used" };
        out.push(diag(li.node_range(&id), DiagnosticSeverity::WARNING, "unused-var",
            format!("variable \"{}\" {} (remove it, or name it _{} to keep it)", name, what, name)));
    }
}

/// unused-local: a `local`/`static` (not const) that no indexed script reads (obj.x, LocalN("x") and strings that
/// may run code count as reads; any variable of that name in any script counts, so this is conservative). Skipped
/// when the definition family uses Local(n) (locals by index) or the name starts with '_'.
pub fn unused_script_vars(index: &Index, info: &FileInfo, out: &mut Vec<Diagnostic>) {
    let current = path_key(&info.path);
    let mut family: Option<Vec<&FileInfo>> = None;
    for v in &info.vars {
        if !matches!(v.kind, super::index::VarKind::Local | super::index::VarKind::Static) || v.name.starts_with('_') {
            continue;
        }
        if info.read_idents.contains(&v.name) || info.string_words.contains(&v.name) || info.own_string_words.contains(&v.name) {
            continue;
        }
        // the index entry of this script may be stale (the open document): count the other scripts only
        let own = index.files.get(&current).map(|f| f.read_idents.contains(&v.name) || f.string_words.contains(&v.name) || f.own_string_words.contains(&v.name)).unwrap_or(false);
        if index.read_count(&v.name) > own as u32 {
            continue;
        }
        // createType1, createType2: the name may be built from "createType" and a number at runtime
        let base = v.name.trim_end_matches(|c: char| c.is_ascii_digit());
        if base.len() < v.name.len() && (index.read_count(base) > 0 || info.read_idents.contains(base) || info.string_words.contains(base)) {
            continue;
        }
        if v.kind == super::index::VarKind::Local {
            let fam = family.get_or_insert_with(|| index.family(info));
            if info.locals_by_index || fam.iter().any(|f| f.locals_by_index) {
                continue;
            }
        }
        let what = if v.kind == super::index::VarKind::Local { "local" } else { "static" };
        out.push(diag(v.range, DiagnosticSeverity::WARNING, "unused-local",
            format!("{} \"{}\" is never read by any script (remove it, or name it _{} to keep it)", what, v.name, v.name)));
    }
}

/// Whether a statement always leaves the code after it: return, break/continue, a block ending in one, an if/else
/// whose branches both leave.
fn exits(s: &Node) -> bool {
    let named: Vec<Node> = named_children(s).into_iter().filter(|c| !matches!(c.kind(), "comment" | "else")).collect();
    match s.kind() {
        "return_statement" | "flow_control_statement" => true,
        "block" => named.last().map(exits).unwrap_or(false),
        "if_statement" => children(s).iter().any(|c| c.kind() == "else") && named.len() == 3 && exits(&named[1]) && exits(&named[2]),
        _ => false,
    }
}

/// unreachable: statements after one that always leaves its block.
pub fn unreachable(root: &Node, src: &str, li: &LineIndex, out: &mut Vec<Diagnostic>) {
    let mut stack = vec![*root];
    while let Some(n) = stack.pop() {
        if n.kind() == "old_function_definition" {
            continue; // old-style labels: the next label starts a new function
        }
        // if (false) / if (0) / while (0): the body never runs; if (true) ... else: the else never runs
        if matches!(n.kind(), "if_statement" | "while_statement") {
            let parts: Vec<Node> = named_children(&n).into_iter().filter(|c| !matches!(c.kind(), "comment" | "else")).collect();
            if let Some(cond) = parts.first() {
                let truth = match cond.kind() {
                    "bool" => Some(node_text(cond, src) == "true"),
                    "nil" => Some(false),
                    "number" => Some(node_text(cond, src).trim_start_matches('0') != ""),
                    _ => None,
                };
                let dead = match truth {
                    Some(false) => parts.get(1),
                    Some(true) if n.kind() == "if_statement" => parts.get(2),
                    _ => None,
                };
                if let Some(d) = dead {
                    let why = if truth == Some(false) { "the condition is always false" } else { "the condition is always true, so the else never runs" };
                    out.push(diag(li.node_range(d), DiagnosticSeverity::WARNING, "unreachable", format!("unreachable code: {}", why)));
                }
            }
        }
        if n.kind() == "block" {
            let stmts: Vec<Node> = named_children(&n).into_iter().filter(|c| c.kind() != "comment").collect();
            if let Some(i) = stmts.iter().position(exits) {
                if let (Some(first), Some(last)) = (stmts.get(i + 1), stmts.last()) {
                    let range = tower_lsp::lsp_types::Range::new(li.node_range(first).start, li.node_range(last).end);
                    let after = match stmts[i].kind() {
                        "return_statement" => "it comes after a return",
                        "flow_control_statement" => "it comes after break/continue",
                        _ => "both branches of the if/else before it leave",
                    };
                    out.push(diag(range, DiagnosticSeverity::WARNING, "unreachable", format!("unreachable code: {}", after)));
                }
            }
        }
        stack.extend(named_children(&n));
    }
}

/// unused-func: a private function nothing calls: no call by name in any script (the
/// open one included), not an engine callback, in no string that may call it, in no ActMap/DefCore *Call=, and no
/// same-named function in its definition family (an overload chain is called through the name).
pub fn unused_funcs(index: &Index, info: &FileInfo, out: &mut Vec<Diagnostic>) {
    let current = path_key(&info.path);
    let mut family: Option<Vec<&FileInfo>> = None;
    for f in &info.funcs {
        // protected/public functions may be called by the engine or other objects in ways we can't see
        let level = match f.visibility.as_deref() {
            Some("private") => DiagnosticSeverity::WARNING,
            _ => continue,
        };
        if super::callers::is_callback(&f.name) || info.calls.iter().any(|c| c.name == f.name) {
            continue;
        }
        if index.call_sites(&f.name).iter().any(|(cf, _)| path_key(&cf.path) != current) {
            continue;
        }
        if !index.ini_uses(&f.name).is_empty() || info.ini_calls.iter().any(|(n, _, _)| *n == f.name) {
            continue;
        }
        if info.string_words.contains(&f.name) || info.own_string_words.contains(&f.name) {
            continue;
        }
        let fam = family.get_or_insert_with(|| index.family(info));
        if index.named_in_strings_of(info, Some(fam), &f.name) {
            continue;
        }
        if fam.iter().any(|g| path_key(&g.path) != current && g.funcs.iter().any(|d| d.name == f.name)) {
            continue;
        }
        if info.funcs.iter().filter(|d| d.name == f.name).count() > 1 {
            continue;
        }
        let msg = if level == DiagnosticSeverity::WARNING {
            format!("private function \"{}\" is never called (no script calls it, no string or ActMap names it)", f.name)
        } else {
            format!("protected function \"{}\" is never called by a script and isn't a known engine callback", f.name)
        };
        out.push(diag(f.name_range, level, "unused-func", msg));
    }
}

/// Hover on a diagnostic code inside a NOLINT / c4lint comment on the line: what it means.
pub fn code_hover(src: &str, line: u32, character: u32) -> Option<String> {
    let text = src.split('\n').nth(line as usize)?;
    let comment = text.find("//").or_else(|| text.find("/*"))?;
    if !(text[comment..].contains("NOLINT") || text[comment..].contains("c4lint:")) {
        return None;
    }
    // the word at the cursor (characters are UTF-16 units; comments with codes are ASCII here)
    let col = (character as usize).min(text.len());
    if col < comment {
        return None;
    }
    let is_code = |c: char| c.is_ascii_alphanumeric() || c == '-';
    let start = text[..col].rfind(|c: char| !is_code(c)).map(|i| i + 1).unwrap_or(0);
    let end = text[col..].find(|c: char| !is_code(c)).map(|i| col + i).unwrap_or(text.len());
    let word = &text[start..end];
    CODES.iter().find(|(c, _)| *c == word).map(|(c, what)| format!("**{}**: {}", c, what))
}
