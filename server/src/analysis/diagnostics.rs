//! Diagnostics for C4Script, modelled on what the engine's parser reports (C4AulParse.cpp):
//! - syntax errors
//! - "unknown identifier" for direct calls and variables that aren't visible from this script
//! - "<Func>: passing N parameters, but only M are used" (an engine warning)
//! - definition IDs that no indexed DefCore.txt defines
//! - constructs that the script's #strict level doesn't allow (none / #strict / #strict 2 / #strict 3)
//! - global funcs: they run in the engine's context, so direct calls only reach global and engine functions and
//!   `local` variables are an error (C4AulParse: `Fn->Owner == &Game.ScriptEngine`)
//! - "stupid func label": a name that looks like an ID (4 chars A-Z 0-9 _) right before '(' or ':' (tokenizer)

use super::engine::{ENGINE, KEYWORDS};
use super::index::{named_children, FileInfo, Index};
use super::text::{node_text, LineIndex};
use std::collections::HashSet;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString};
use tree_sitter::{Node, Tree};

pub const SOURCE: &str = "c4script";

fn diag(range: tower_lsp::lsp_types::Range, sev: DiagnosticSeverity, code: &str, msg: String) -> Diagnostic {
    Diagnostic {
        range,
        severity: Some(sev),
        code: Some(NumberOrString::String(code.to_string())),
        source: Some(SOURCE.to_string()),
        message: msg,
        ..Default::default()
    }
}

pub fn diagnose(index: &Index, info: &FileInfo, src: &str, tree: &Tree) -> Vec<Diagnostic> {
    let li = LineIndex::new(src);
    let mut out = vec![];
    syntax_errors(&tree.root_node(), src, &li, &mut out);
    let strict = strict_level(&tree.root_node(), src);
    if strict == 0 && !info.is_system {
        out.push(diag(Default::default(), DiagnosticSeverity::INFORMATION, "no-strict",
            "no #strict directive: legacy (non-strict) syntax rules apply; new scripts should start with #strict 2".to_string()));
    }
    let unresolved_scope = index.has_unresolved_targets(info);
    let mut ctx = Ctx { index, info, src, li: &li, out: &mut out, locals: HashSet::new(), strict, func: None,
        func_start: (0, 0), in_global: false, id_label_error: false, unresolved_scope, types: None };
    ctx.walk(tree.root_node());
    check_string_table(info, src, &li, &mut out);
    // The engine doesn't report errors in System.c4g scripts that are pure #appendto scripts (C4AulParse Warn/
    // ParseFn: "do not show errors for System.c4g scripts that appear to be pure #appendto scripts"), but the
    // broken function still isn't compiled: report them as warnings.
    if info.is_system && !info.appendto.is_empty() {
        for d in out.iter_mut().filter(|d| d.severity == Some(DiagnosticSeverity::ERROR)) {
            d.severity = Some(DiagnosticSeverity::WARNING);
            d.message.push_str(" (System.c4g #appendto script: the engine hides this error, but the function isn't compiled)");
        }
    }
    out
}

/// `$Name$` references: the engine warns "string table entry not found" when the table (StringTbl.txt /
/// StringTbl<Language>.txt next to the script) has no such entry; missing in only some languages: info.
fn check_string_table(info: &FileInfo, src: &str, li: &LineIndex, out: &mut Vec<Diagnostic>) {
    let refs = super::strings::refs(src);
    if refs.is_empty() {
        return;
    }
    let tables = super::strings::tables_for(&info.path);
    for (name, start, end) in refs {
        let range = tower_lsp::lsp_types::Range::new(li.byte_to_lsp(start), li.byte_to_lsp(end));
        let missing: Vec<String> = tables.iter().filter(|t| !t.entries.contains_key(&name))
            .map(|t| t.path.file_name().unwrap_or_default().to_string_lossy().to_string()).collect();
        if tables.is_empty() {
            out.push(diag(range, DiagnosticSeverity::WARNING, "string-table",
                format!("string table entry not found: \"{}\" (no StringTbl*.txt next to this script)", name)));
        } else if missing.len() == tables.len() {
            out.push(diag(range, DiagnosticSeverity::WARNING, "string-table",
                format!("string table entry not found: \"{}\" (not in {})", name, missing.join(", "))));
        } else if !missing.is_empty() {
            out.push(diag(range, DiagnosticSeverity::INFORMATION, "string-table",
                format!("${}$ is missing in {}: players with that language get a warning and the raw text", name, missing.join(", "))));
        }
    }
}

/// 0 = no #strict, 1 = #strict, 2 = #strict 2, 3 = #strict 3 (C4AulScriptStrict)
pub fn strict_level(root: &Node, src: &str) -> u8 {
    for c in named_children(root) {
        if c.kind() == "pragma_strict" {
            let t = node_text(&c, src);
            return t.trim_start_matches("#strict").trim().parse().unwrap_or(1);
        }
    }
    0
}

fn syntax_errors(n: &Node, src: &str, li: &LineIndex, out: &mut Vec<Diagnostic>) {
    if !n.has_error() {
        return;
    }
    if n.is_missing() {
        out.push(diag(li.node_range(n), DiagnosticSeverity::ERROR, "syntax", format!("syntax error: missing '{}'", n.kind())));
        return;
    }
    if n.is_error() {
        let text: String = node_text(n, src).chars().take(40).collect();
        let text = text.lines().next().unwrap_or("").trim().to_string();
        out.push(diag(li.node_range(n), DiagnosticSeverity::ERROR, "syntax", format!("syntax error near '{}'", text)));
        return;
    }
    let mut c = n.walk();
    for child in n.children(&mut c) {
        syntax_errors(&child, src, li, out);
    }
}

struct Ctx<'a> {
    index: &'a Index,
    info: &'a FileInfo,
    src: &'a str,
    li: &'a LineIndex<'a>,
    out: &'a mut Vec<Diagnostic>,
    /// parameters and `var`s of the function being walked
    locals: HashSet<String>,
    strict: u8,
    /// name of the function being walked
    func: Option<String>,
    /// start (row, column) of the function being walked
    func_start: (usize, usize),
    /// the function being walked is a `global func`
    in_global: bool,
    /// a "stupid func label" error was reported: the engine aborts the script there, so only the first one counts
    id_label_error: bool,
    /// an #include/#appendto target of this script isn't indexed: its functions/locals are unknown
    unresolved_scope: bool,
    /// inferred types of the function being walked (types.rs), when type checks are on
    types: Option<super::types::FuncTypes>,
}

impl<'a> Ctx<'a> {
    fn strict_error(&mut self, n: &Node, msg: &str) {
        self.out.push(diag(self.li.node_range(n), DiagnosticSeverity::ERROR, "strict", msg.to_string()));
    }

    fn walk(&mut self, n: Node) {
        if n.is_error() {
            return;
        }
        match n.kind() {
            "function_definition" | "old_function_definition" => {
                self.locals = function_locals(&n, self.src);
                self.func = n.child_by_field_name("name").map(|x| node_text(&x, self.src).to_string());
                self.func_start = (n.start_position().row, n.start_position().column);
                self.in_global = n.child_by_field_name("visibility").map(|v| node_text(&v, self.src) == "global").unwrap_or(false);
                if let Some(name) = n.child_by_field_name("name") {
                    self.check_id_label(&name);
                }
                if type_checks_enabled() {
                    self.types = Some(super::types::infer_function(self.index, self.info, self.src, n, self.strict));
                }
                for c in named_children(&n) {
                    self.walk(c);
                }
                self.types = None;
                self.locals.clear();
                self.func = None;
                self.in_global = false;
                return;
            }
            "array" if self.strict == 0 => self.strict_error(&n, "unexpected '[' (array literals need #strict)"),
            "map" if self.strict < 3 => self.strict_error(&n, "unexpected '{' (map literals need #strict 3)"),
            "array_append" if self.strict == 0 => self.strict_error(&n, "unexpected '[' (arr[] = x needs #strict)"),
            // nil-safe navigation obj?->F(), map?.key, arr?[i]: C4AulParse ATT_QMARK
            "arrow_expression" | "map_access" if self.strict < 3 && super::index::children(&n).iter().any(|c| c.kind() == "?") => {
                self.strict_error(&n, "unexpected '?' (nil-safe navigation needs #strict 3)")
            }
            "map_access" if self.strict < 3 && super::index::children(&n).iter().any(|c| c.kind() == ".") => {
                self.strict_error(&n, "unexpected '.' (map access with '.' needs #strict 3)")
            }
            "nil" if self.strict < 3 => self.strict_error(&n, "unknown identifier: nil (nil is a keyword only in #strict 3)"),
            "method_call" => self.check_call(&n),
            "identifier" => self.check_identifier(&n),
            "id" => self.check_id(&n),
            // #include/#appendto: only the target ID matters (it may not be loaded by this scenario)
            "include" | "appendto" => {
                for c in named_children(&n).into_iter().filter(|c| c.kind() == "id") {
                    self.check_id(&c);
                }
                return;
            }
            "comment" | "string" | "func_desc" | "pragma_strict" => return,
            _ => {}
        }
        for c in named_children(&n) {
            self.walk(c);
        }
    }

    fn check_call(&mut self, n: &Node) {
        let name_node = match n.child_by_field_name("name") {
            Some(x) => x,
            None => return,
        };
        let name = node_text(&name_node, self.src);
        self.check_id_label(&name_node);
        if (name == "inherited" || name == "_inherited") && self.strict == 0 {
            self.out.push(diag(self.li.node_range(&name_node), DiagnosticSeverity::ERROR, "strict",
                "inherited disabled; use #strict syntax!".to_string()));
            return;
        }
        if name == "inherited" {
            if let Some(f) = self.func.clone() {
                if !self.has_overloaded(&f) {
                    self.out.push(diag(self.li.node_range(&name_node), DiagnosticSeverity::ERROR, "inherited",
                        format!("inherited function not found, use _inherited to call failsafe ({} isn't defined in an #include'd/#appendto'd script)", f)));
                }
            }
            return;
        }
        if KEYWORDS.contains(&name) {
            return;
        }
        let args = n.child_by_field_name("args").map(|a| named_children(&a).iter().filter(|c| c.kind() != "comment").count()).unwrap_or(0);
        let range = self.li.node_range(&name_node);
        if super::index::is_arrow_call(n) {
            // obj->EngineFunc(a, ...): the arguments fill the parameters from the first one on - the engine doesn't
            // put obj into a leading "object pObj" parameter (obj->MakeCrewMember(plr) passes plr as the object)
            if n.child_by_field_name("id").is_none() && self.index.reachable_funcs(&self.info.path, name).is_empty() {
                if let Some(ef) = ENGINE.functions.get(name) {
                    self.check_arg_types(n, name, &engine_params(ef), true, true);
                }
            }
            // resolved at runtime, but the engine checks at compile time that some loaded script (or the engine)
            // has a function of that name: obj->F() without '~' is "direct object call: function F not found"
            // (C4AulParse: Engine->GetFirstFunc). ID::F() calls are checked differently: skip them.
            if n.child_by_field_name("id").is_none()
                && self.index.reachable_funcs(&self.info.path, name).is_empty()
                && !ENGINE.functions.contains_key(name)
            {
                let failsafe = n.parent().map(|p| super::index::children(&p).iter().any(|c| c.kind() == "~")).unwrap_or(false);
                if !failsafe && self.index.scope_packs_indexed(&self.info.path) {
                    self.out.push(diag(range, DiagnosticSeverity::ERROR, "direct-call",
                        format!("direct object call: function {} not found (no script this scenario loads defines it; ->~{}() calls it only if it exists)", name, name)));
                } else {
                    self.out.push(diag(range, DiagnosticSeverity::INFORMATION, "unknown-method",
                        format!("no indexed script defines a function '{}' (called via ->)", name)));
                }
            }
            return;
        }
        // Script functions: the engine never checks their parameter count (GetParCount() is always 10).
        // A global func looks functions up in the engine only: global funcs, not this script's own or #include'd ones.
        let script_funcs = if self.in_global {
            self.index.global_funcs(&self.info.path, name)
        } else {
            self.index.visible_funcs(self.info, name)
        };
        if !script_funcs.is_empty() {
            // typed parameters of script functions are converted like the engine's (C4AulExec): checked when every
            // definition the call could reach agrees on the type
            let params: Vec<Vec<(String, String)>> = script_funcs.iter()
                .map(|(_, f)| f.params.iter().map(|(t, n)| (t.clone().unwrap_or_default(), n.clone())).collect())
                .collect();
            if params.windows(2).all(|w| w[0] == w[1]) {
                self.check_arg_types(n, name, &params[0], false, false);
            }
            return;
        }
        // Old scripts call constants like functions: COMD_Left() (only without #strict 2)
        if ENGINE.constants.contains_key(name) || !self.index.visible_vars(self.info, name).is_empty() {
            if self.strict >= 2 {
                self.out.push(diag(range, DiagnosticSeverity::ERROR, "strict",
                    format!("{} is a constant, not a function: write {} without () in #strict 2 and later", name, name)));
            }
            return;
        }
        if let Some(ef) = ENGINE.functions.get(name) {
            if self.strict >= 3 {
                self.check_literal_nothing(n, name, ef);
            }
            self.check_arg_types(n, name, &engine_params(ef), false, true);
            if let (true, Some(ps)) = (ef.arity_known, &ef.params) {
                if args > ps.len() {
                    self.out.push(diag(range, DiagnosticSeverity::WARNING, "too-many-params",
                        format!("{}: passing {} parameters, but only {} are used", name, args, ps.len())));
                }
            }
            return;
        }
        if self.in_global && !self.index.visible_funcs(self.info, name).is_empty() {
            if !self.unresolved_scope {
                self.out.push(diag(range, DiagnosticSeverity::ERROR, "unknown-identifier",
                    format!("unknown identifier: {} (a global func only sees global and engine functions; {} isn't global)", name, name)));
            }
            return;
        }
        let elsewhere = self.index.all_funcs(name);
        let loaded = elsewhere.iter().find(|(f, _)| self.index.visible_from(&self.info.path, &f.path));
        let msg = match (loaded, elsewhere.first()) {
            (None, Some((f, _))) => format!(
                "unknown identifier: {} (only defined in {}, which this scenario doesn't load: not in its folder, its .c4f folders or the packs of its [Definitions])",
                name, f.path.display()),
            (Some((f, _)), _) => format!(
                "unknown identifier: {} (defined in {} but not visible here: not global, not in this script, its #include chain or #appendto scripts)",
                name, f.path.display()),
            (None, None) => format!("unknown identifier: {} (not an engine function and not defined in any indexed script)", name),
        };
        if self.unresolved_scope {
            return; // the missing #include/#appendto target may define it
        }
        self.out.push(diag(range, DiagnosticSeverity::ERROR, "unknown-identifier", msg));
    }

    /// #strict 3: `0`/`true`/`false` are int/bool values, no longer "nothing". Passed to an engine parameter of type
    /// object/string/array/map the call fails at runtime ("call to "F" parameter N: got "int", but expected
    /// "object"!", C4Value::C4ScriptCnvMap: int/bool -> object/string/array/map = NEVER; int -> id is allowed).
    fn check_literal_nothing(&mut self, call: &Node, name: &str, ef: &super::engine::EngineFunc) {
        let (Some(args), Some(params)) = (call.child_by_field_name("args"), &ef.params) else { return };
        let args: Vec<Node> = named_children(&args).into_iter().filter(|c| c.kind() != "comment").collect();
        for (i, (arg, p)) in args.iter().zip(params.iter()).enumerate() {
            let literal = match arg.kind() {
                "number" => "an int",
                "bool" => "a bool",
                _ => continue,
            };
            if !matches!(p.ty.as_str(), "object" | "string" | "array" | "map") {
                continue;
            }
            let text = node_text(arg, self.src);
            self.out.push(diag(self.li.node_range(arg), DiagnosticSeverity::WARNING, "strict3-nothing",
                format!("{}: parameter {} ({}) expects {}, but {} is {} in #strict 3 (not 'nothing'): pass nil instead; the call fails at runtime",
                    name, i + 1, p.name, p.ty, text, literal)));
        }
    }

    /// Arguments whose type is certain and that the engine can never convert to the parameter's type: the call fails
    /// at runtime ("call to "F" parameter N: got "object", but expected "string"!"; C4AulExec
    /// CheckConvertFunctionParameters with C4Value::C4ScriptCnvMap, strict conversion). Below #strict 3 a falsy
    /// value (0, false, nil) is passed as "nothing" (convertToAnyEagerly), so only values that are never falsy count
    /// there; in #strict 3 int literals are left to strict3-nothing.
    fn check_arg_types(&mut self, call: &Node, name: &str, params: &[(String, String)], arrow: bool, engine: bool) {
        if !type_checks_enabled() {
            return;
        }
        let Some(args) = call.child_by_field_name("args") else { return };
        let args: Vec<Node> = named_children(&args).into_iter().filter(|c| c.kind() != "comment").collect();
        for (i, (arg, (ty, pname))) in args.iter().zip(params.iter()).enumerate() {
            let got = match self.static_type(arg) {
                Some((got, truthy)) => {
                    if !truthy && self.strict < 3 {
                        continue;
                    }
                    got.to_string()
                }
                None => {
                    // inferred (types.rs): every possible type fails to convert, and none of them can be "nothing"
                    // (nil converts to anything; below #strict 3 also 0, false and "" do)
                    use super::types::*;
                    let Some(t) = self.types.as_ref().and_then(|ft| ft.at.get(&arg.id()).copied()) else { continue };
                    let maybe_falsy = if self.strict >= 3 { NIL } else { NIL | INT | BOOL | ID | STRING };
                    if t == NONE || t == ANY || t & maybe_falsy != 0 {
                        continue;
                    }
                    if !members(t).iter().all(|m| never_converts(m, ty.as_str())) {
                        continue;
                    }
                    display(t)
                }
            };
            let got = got.as_str();
            if self.strict >= 3 && engine && !arrow && matches!(arg.kind(), "number" | "bool") {
                continue; // reported as strict3-nothing
            }
            if !never_converts(got, ty.as_str()) {
                continue;
            }
            let hint = if arrow && i == 0 && ty == "object" {
                format!(" (obj->{}(...) doesn't fill the object parameter: write {}(obj, ...))", name, name)
            } else {
                String::new()
            };
            self.out.push(diag(self.li.node_range(arg), DiagnosticSeverity::ERROR, "arg-type",
                format!("call to \"{}\" parameter {}: got \"{}\", but expected \"{}\" ({}): fails at runtime{}",
                    name, i + 1, got, ty, pname, hint)));
            break; // the engine stops at the first parameter it can't convert
        }
    }

    /// The type of an argument when it is certain, and whether the value can never be falsy.
    fn static_type(&self, arg: &Node) -> Option<(&'static str, bool)> {
        let text = node_text(arg, self.src);
        match arg.kind() {
            "string" => Some(("string", text.len() > 2)),
            "number" => Some(("int", text.trim_start_matches("0x").trim_start_matches('0') != "")),
            "bool" => Some(("bool", text == "true")),
            "array" => Some(("array", true)),
            "map" => Some(("map", true)),
            // 4-character tokens: digits only (1000) is a number, the grammar lexes it as an ID
            "id" if text.chars().all(|c| c.is_ascii_digit()) => Some(("int", text.trim_start_matches('0') != "")),
            "id" => Some(("id", true)),
            "identifier" if text == "this" => Some(("object", true)),
            "method_call" => {
                let name = node_text(&arg.child_by_field_name("name")?, self.src);
                if name == "this" {
                    return Some(("object", true));
                }
                if name == "Format" && !super::index::is_arrow_call(arg) && self.index.visible_funcs(self.info, name).is_empty() {
                    return Some(("string", true));
                }
                None
            }
            _ => None,
        }
    }

    /// Is there another definition of function `f` that `inherited` could call?
    fn has_overloaded(&self, f: &str) -> bool {
        let own = super::index::path_key(&self.info.path);
        // a second definition of a function in the same script overloads the first one (C4Aul: later wins)
        let (row, col) = self.func_start;
        self.info.funcs.iter().any(|x| x.name == f && (x.range.start.line as usize, x.range.start.character as usize) < (row, col))
            || self.index.scope_files(self.info).iter().any(|file| super::index::path_key(&file.path) != own && file.funcs.iter().any(|x| x.name == f))
            || !self.index.global_funcs(&self.info.path, f).is_empty()
            || ENGINE.functions.contains_key(f)
    }

    fn check_identifier(&mut self, n: &Node) {
        let parent = match n.parent() {
            Some(p) => p,
            None => return,
        };
        // declarations and non-variable positions
        match parent.kind() {
            "method_call" | "function_definition" | "old_function_definition" | "param" | "visibility" | "map_key" | "map_entry" => return,
            "var_assignment" if named_children(&parent).first().map(|f| f.id() == n.id()).unwrap_or(false) => return,
            "map_access" => {
                // obj.key: the key is not a variable
                let kids = super::index::children(&parent);
                if let Some(pos) = kids.iter().position(|k| k.id() == n.id()) {
                    if pos > 0 && kids[pos - 1].kind() == "." {
                        return;
                    }
                }
            }
            _ => {}
        }
        let name = node_text(n, self.src);
        if self.in_global && !self.locals.contains(name)
            && self.index.visible_vars(self.info, name).iter().any(|(_, v)| v.kind == super::index::VarKind::Local)
        {
            self.out.push(diag(self.li.node_range(n), DiagnosticSeverity::ERROR, "local-in-global",
                format!("using local variable in global function! ({} is a local of the script; a global func has no object context)", name)));
            return;
        }
        if self.locals.contains(name)
            || KEYWORDS.contains(&name)
            || ENGINE.constants.contains_key(name)
            || ENGINE.functions.contains_key(name)
            || !self.index.visible_vars(self.info, name).is_empty()
            || !self.index.visible_funcs(self.info, name).is_empty()
        {
            return;
        }
        // a label like "Script1:" in old scenario scripts
        if let Some(next) = n.next_sibling() {
            if next.kind() == ":" {
                return;
            }
        }
        if self.unresolved_scope {
            return; // the missing #include/#appendto target may define it as a local
        }
        self.out.push(diag(self.li.node_range(n), DiagnosticSeverity::ERROR, "unknown-identifier",
            format!("unknown identifier: {} (no local variable, parameter, local/static variable or constant with this name)", name)));
    }

    /// The engine's tokenizer: an identifier that looks like an ID (exactly 4 chars of A-Z 0-9 _) directly followed by
    /// '(' or a label ':' gives "stupid func label" (error from #strict 2 on, a warning below).
    fn check_id_label(&mut self, name: &Node) {
        let text = node_text(name, self.src);
        if text.len() != 4 || !text.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_') {
            return;
        }
        let rest = &self.src.as_bytes()[name.end_byte()..];
        let label = match rest.first() {
            Some(b'(') => true,
            Some(b':') => rest.get(1) != Some(&b':'),
            _ => false,
        };
        if !label {
            return;
        }
        let sev = if self.strict >= 2 {
            if self.id_label_error {
                return;
            }
            self.id_label_error = true;
            DiagnosticSeverity::ERROR
        } else {
            DiagnosticSeverity::WARNING
        };
        self.out.push(diag(self.li.node_range(name), sev, "id-label",
            format!("stupid func label: {} (looks like a definition ID: use a name that isn't 4 capitals/digits)", text)));
    }

    fn check_id(&mut self, n: &Node) {
        let id = node_text(n, self.src);
        if id.chars().all(|c| c.is_ascii_digit()) {
            return; // a number like 1000, which the grammar's ID pattern also matches
        }
        let std = standard_id(id);
        if self.index.id_for(id, &self.info.path).is_some()
            || std.map(|s| self.index.loads_pack(&self.info.path, &s.tlc)).unwrap_or(false)
            || ENGINE.constants.contains_key(id)
            || !self.index.visible_vars(self.info, id).is_empty()
        {
            return;
        }
        let msg = if let Some(i) = self.index.id_any(id) {
            format!("definition {} is not loaded by this scenario: it's defined in {}, which isn't in the scenario, its .c4f folders or the packs of its [Definitions]",
                id, i.defcore.parent().unwrap_or(&i.defcore).display())
        } else if let Some(s) = std {
            format!("definition {} ({}) is from {}, which this scenario doesn't load: add Definition<N>={} to its Scenario.txt [Definitions]",
                id, s.name, s.tlc, s.tlc)
        } else {
            format!("unknown definition ID: {} (no DefCore.txt in the indexed folders and no standard pack has id={})", id, id)
        };
        self.out.push(diag(self.li.node_range(n), DiagnosticSeverity::WARNING, "unknown-id", msg));
    }
}

/// IDs of the standard packs (Objects.c4d, Knights.c4d, Western.c4d, ...) known without indexing them
pub fn standard_id(id: &str) -> Option<&'static crate::workspace::IdInfo> {
    crate::workspace::Workspace::new().get_ids().iter().find(|i| i.id == id)
}

/// Parameters and all `var` declarations of a function (C4Script vars are function-scoped).
fn function_locals(f: &Node, src: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut stack = vec![*f];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "param" => {
                if let Some(name) = n.child_by_field_name("name") {
                    out.insert(node_text(&name, src).to_string());
                }
            }
            "var_assignment" => {
                if let Some(id) = named_children(&n).first() {
                    out.insert(node_text(id, src).to_string());
                }
            }
            "for_statement" => {
                // for (var x in list) / for (var i, x in list): identifiers right after 'var'
                let kids = super::index::children(&n);
                for w in kids.windows(2) {
                    if w[0].kind() == "var" && w[1].kind() == "identifier" {
                        out.insert(node_text(&w[1], src).to_string());
                    }
                }
                for w in kids.windows(3) {
                    if w[0].kind() == "identifier" && w[1].kind() == "," && w[2].kind() == "identifier" {
                        out.insert(node_text(&w[2], src).to_string());
                    }
                }
                // 'var' is optional: for (x in list) / for (k, v in map) also declare x / k, v
                // (C4AulParse Parse_ForEach adds the names to the function's variables)
                for w in kids.windows(2) {
                    if w[0].kind() == "identifier" && w[1].kind() == "in" {
                        out.insert(node_text(&w[0], src).to_string());
                    }
                }
                for w in kids.windows(4) {
                    if w[0].kind() == "identifier" && w[1].kind() == "," && w[2].kind() == "identifier" && w[3].kind() == "in" {
                        out.insert(node_text(&w[0], src).to_string());
                    }
                }
            }
            _ => {}
        }
        stack.extend(named_children(&n));
    }
    out
}

/// C4Value::C4ScriptCnvMap: conversions that fail even in old scripts ("NEVER!", "Senseless") and the strict
/// "#strict forbid" ones (engine parameters are always converted strictly).
fn never_converts(from: &str, to: &str) -> bool {
    if to.contains('&') {
        return false; // reference parameters (SetLength(&array, ...)): checked differently by the engine
    }
    match (from, to) {
        (_, "any") | (_, "") => false,
        (a, b) if a == b => false,
        (_, "bool") => false,
        ("int", "id") => false,
        ("bool", "int") => false,
        ("int" | "bool" | "id", "object" | "string" | "array" | "map") => true,
        ("bool", "id") => true,
        ("id" | "object" | "string", "int") => true,
        ("object", "id" | "string" | "array" | "map") => true,
        ("string", "id" | "object" | "array" | "map") => true,
        ("array", _) | ("map", _) => true,
        _ => false,
    }
}

fn engine_params(ef: &super::engine::EngineFunc) -> Vec<(String, String)> {
    // the engine converts to the C++ parameter type (C4Value = any), not the documented one
    ef.params.iter().flatten().map(|p| (if p.etype.is_empty() { "any".to_string() } else { p.etype.clone() }, p.name.clone())).collect()
}

/// Type checks (arg-type) can be turned off: LSP initializationOptions `"typeChecks": false`, c4check
/// `--no-type-checks`, or the environment variable C4LS_TYPE_CHECKS=0.
pub static TYPE_CHECKS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn type_checks_enabled() -> bool {
    TYPE_CHECKS.load(std::sync::atomic::Ordering::Relaxed) && std::env::var("C4LS_TYPE_CHECKS").map(|v| v != "0").unwrap_or(true)
}
