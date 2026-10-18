//! Type inference for C4Script: flow-sensitive, per function.
//!
//! Every value has a set of possible types (a union like `int | string | nil`). Inside a function the analysis walks
//! the statements in order: an assignment replaces a variable's type from that point on, both branches of an `if`
//! are analysed and merged afterwards, loops are repeated until the types stop growing, and `return`/`break`/
//! `continue` end a path. Types come from literals, `this`, typed parameters, the engine's C++ return types and the
//! return statements of script functions (summaries, computed on demand across scripts and cached).
//! Engine rules followed (C4AulExec): `&&`/`||` give one of their operands from #strict 2 on (a bool before),
//! `a ?? b` gives a unless it is nil, arithmetic gives int, comparisons bool, `..` a string (or an array).
//! Arrays carry the union of their element types (`array<int>`: from literals, `a[] = x`, `a[i] = x`, `..`,
//! FindObjects), so `for (var x in a)` types x; reading `a[i]` adds nil (out of range).
//! Script variables (`local`, `static`, `static const`) get the union of every value assigned to them: a `local` from
//! the scripts of its definition (with its #include/#appendto family) plus `obj.name = x` / `LocalN("name") = x`
//! writes anywhere, a `static` from every indexed script that assigns it (field_types_enabled() switches this off).
//! Unknown is `any` (untyped parameters, Par(), map values).

use super::engine::ENGINE;
use super::index::{children, is_arrow_call, named_children, new_parser, path_key, FileInfo, FuncDef, Index};
use super::text::{decode_bytes, node_text};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tree_sitter::{Node, Tree};

pub type Ty = u16;
pub const NONE: Ty = 0;
pub const NIL: Ty = 1;
pub const INT: Ty = 2;
pub const BOOL: Ty = 4;
pub const ID: Ty = 8;
pub const OBJECT: Ty = 16;
pub const STRING: Ty = 32;
pub const ARRAY: Ty = 64;
pub const MAP: Ty = 128;
/// low byte: the kinds above; high byte: the element types of an array (meaningful with ARRAY, 0xFF = unknown)
pub const ANY: Ty = 0xFFFF;
pub const ARRAY_ANY: Ty = ARRAY | 0xFF00;

/// An array whose elements have type `elem` (the elements' own element types are not kept).
pub fn array_of(elem: Ty) -> Ty {
    ARRAY | ((elem & 0xFF) << 8)
}

/// The kinds of a type without array element types.
pub fn kinds(t: Ty) -> Ty {
    t & 0xFF
}

/// Element type of an array type (nested arrays have unknown elements).
pub fn elements(t: Ty) -> Ty {
    let e = (t >> 8) & 0xFF;
    if e == 0xFF {
        ANY
    } else if e & ARRAY != 0 {
        e | 0xFF00
    } else {
        e
    }
}

const NAMES: [(Ty, &str); 8] =
    [(INT, "int"), (BOOL, "bool"), (ID, "id"), (OBJECT, "object"), (STRING, "string"), (ARRAY, "array"), (MAP, "map"), (NIL, "nil")];

/// The type a C4Script type name stands for (`object`, `int &`, ...); unknown names are `any`.
pub fn from_name(s: &str) -> Ty {
    match s.trim().trim_start_matches('&').trim_end_matches('&').trim() {
        "int" => INT,
        "bool" => BOOL,
        "id" => ID,
        "object" => OBJECT,
        "string" => STRING,
        "array" => ARRAY_ANY,
        "map" => MAP,
        "void" => NIL,
        _ => ANY,
    }
}

/// Single type names of a set (no nil), for the conversion checks.
pub fn members(t: Ty) -> Vec<&'static str> {
    NAMES.iter().filter(|(b, n)| t & b != 0 && *n != "nil").map(|(_, n)| *n).collect()
}

pub fn display(t: Ty) -> String {
    if t == ANY {
        return "any".to_string();
    }
    if t == NONE {
        return "never (unreachable)".to_string();
    }
    NAMES.iter()
        .filter(|(b, _)| t & b != 0)
        .map(|(b, n)| {
            if *b != ARRAY {
                return n.to_string();
            }
            let e = elements(t);
            if e == ANY {
                "array".to_string()
            } else if e == NONE {
                "array<empty>".to_string()
            } else {
                format!("array<{}>", display(e))
            }
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Return type of an engine function, from its C++ signature: objects, strings, arrays, maps and IDs can be
/// nil (failure); C4Value returns are `any`.
pub fn engine_return(name: &str) -> Option<Ty> {
    let ef = ENGINE.functions.get(name)?;
    match name {
        "Format" => return Some(STRING),
        "FindObjects" => return Some(array_of(OBJECT)),
        "CreateArray" => return Some(array_of(NIL)),
        // creating an object fails only in exceptional cases (invalid ID, removed in Initialize): assumed to work
        // (switch: diagnostics::assume_create_succeeds) -> object, not object | nil
        "CreateObject" | "CreateContents" | "CreateConstruction" | "PlaceAnimal" | "PlaceVegetation" | "ComposeContents"
            if super::diagnostics::assume_create_succeeds() => return Some(OBJECT),
        _ => {}
    }
    let t = from_name(if ef.ertype.is_empty() { &ef.rtype } else { &ef.ertype });
    if t == ANY {
        return Some(ANY);
    }
    Some(if t & (OBJECT | STRING | ARRAY | MAP | ID) != 0 { t | NIL } else { t })
}

type State = HashMap<String, Ty>;

fn merge(a: Option<State>, b: Option<State>) -> Option<State> {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some(mut a), Some(b)) => {
            // a call fact ("Contained() isn't nil") holds after the join only if it holds on both paths
            a.retain(|k, _| !k.starts_with(CALL_FACT) || b.contains_key(k));
            for (k, v) in b {
                if k.starts_with(CALL_FACT) && !a.contains_key(&k) {
                    continue;
                }
                *a.entry(k).or_insert(NONE) |= v;
            }
            Some(a)
        }
    }
}

/// The state key of the call fact for a call written `text` (no spaces), e.g. "Contained()".
pub fn call_fact(text: &str) -> String {
    format!("{}{}", CALL_FACT, text)
}

/// A call fact that means the same in a function called on the same object: no variables in its arguments.
fn transferable(key: &str) -> bool {
    key.trim_start_matches(CALL_FACT).split(|c| c == '(' || c == ',' || c == ')').skip(1).filter(|a| !a.is_empty())
        .all(|a| a.chars().all(|c| c.is_ascii_digit()) || NIL_SOURCES.contains(&a)
            || (a.len() == 4 && a.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')))
}

/// State key prefix of a call fact: the type a call like `Contained()` / `GetCrew(iPlr)` has after a guard
/// (`if (!Contained()) return;`), keyed by the call's text.
const CALL_FACT: &str = "\u{1}c:";

/// Engine functions that can return nil when they find nothing (NIL_SOURCES of diagnostics.rs) and whose guarded
/// calls are remembered.
pub const NIL_SOURCES: &[&str] = &["FindObject", "FindObject2", "FindObjectOwner", "FindContents", "FindBase", "FindOtherContents",
    "Contained", "Contents", "ScrollContents", "GetCursor", "GetViewCursor", "GetCaptain", "GetCrew", "GetHiRank", "GetActionTarget"];

/// The call facts (by called function) a call can end: actions change GetActionTarget(), containment changes
/// Contained()/Contents(), removing/changing objects anything. None: no effect.
fn fact_breaks(name: &str) -> Option<&'static [&'static str]> {
    const ACTION: &[&str] = &["GetActionTarget"];
    const CONTAINMENT: &[&str] = &["Contained", "Contents", "FindContents", "ScrollContents", "FindOtherContents"];
    const ALL: &[&str] = &[];
    match name {
        "SetAction" | "SetActionTargets" | "ObjectSetAction" | "SetCommand" | "AddCommand" | "AppendCommand" => Some(ACTION),
        "Exit" | "Enter" | "Collect" => Some(CONTAINMENT),
        "RemoveObject" | "ChangeDef" | "Split2Components" => Some(ALL),
        _ => None,
    }
}

/// Result for one function: the type of every evaluated expression and variable use (by node id), the type
/// each variable has at its declaration/assignment, and the function's return type.
#[derive(Default)]
pub struct FuncTypes {
    pub at: HashMap<usize, Ty>,
    pub returns: Ty,
    /// values assigned to script variables (identifiers that aren't parameters/vars of the function)
    pub writes: HashMap<String, Ty>,
    /// values assigned to other objects' locals: obj.name = x, LocalN("name", obj) = x, with the object's
    /// definition when the expression shows it (this, CreateObject(ID, ...), FindObject(Find_ID(ID)), ...)
    pub member_writes: HashMap<String, Vec<(Option<String>, Ty)>>,
    /// direct calls (F() / this->F()): the call facts holding there that also make sense in the callee (no
    /// variables in them), for callers.rs
    pub site_facts: HashMap<usize, Vec<String>>,
    /// x += 1, x++, x ..= y: the type x had before (`at` holds the new one), for diagnostics' op-type
    pub read_at: HashMap<usize, Ty>,
    /// per parameter: for a reference parameter (&x) the type it holds when the function ends (union over the
    /// exits that assigned it) and whether some exit leaves the caller's value untouched; others (NONE, false)
    pub ref_out: Vec<(Ty, bool)>,
}

/// State keys shadowing a reference parameter: the type written to it on this path, and "not written on this path"
/// (non-NONE). Names can't contain these characters.
fn ref_written(name: &str) -> String {
    format!("\u{1}w:{}", name)
}
fn ref_untouched(name: &str) -> String {
    format!("\u{1}u:{}", name)
}

/// What one function writes to script variables (FuncTypes::writes / member_writes).
pub struct Writes {
    writes: HashMap<String, Ty>,
    member_writes: HashMap<String, Vec<(Option<String>, Ty)>>,
}

/// Inferred type of a script variable.
#[derive(Clone, Copy, Debug)]
pub struct FieldType {
    pub ty: Ty,
    /// number of assigning functions and of scripts they are in
    pub writers: usize,
    pub scripts: usize,
}

lazy_static::lazy_static! {
    static ref SUMMARIES: Mutex<HashMap<String, Ty>> = Mutex::new(HashMap::new());
    /// reference parameters of script functions: what each one holds at the end (FuncTypes::ref_out)
    static ref REF_SUMMARIES: Mutex<HashMap<String, Vec<(Ty, bool)>>> = Mutex::new(HashMap::new());
    /// script variable types: "local|<def script>|name" / "static|name" -> (type, assignments, scripts)
    static ref FIELDS: Mutex<HashMap<String, FieldType>> = Mutex::new(HashMap::new());
    /// what a function writes to script variables (FuncTypes::writes/member_writes), by "<script>|func|line|depth"
    static ref WRITES: Mutex<HashMap<String, Arc<Writes>>> = Mutex::new(HashMap::new());
    static ref IN_PROGRESS: Mutex<HashSet<String>> = Mutex::new(HashSet::new());
    /// parsed scripts whose functions were summarised (source, tree)
    static ref TREES: Mutex<HashMap<String, Arc<(String, Tree)>>> = Mutex::new(HashMap::new());
}

/// Key of an in-progress set (recursion detection): per thread, since files are checked in parallel and another
/// thread working on the same function is no recursion.
pub(crate) fn busy_key(key: &str) -> String {
    format!("{:?}|{}", std::thread::current().id(), key)
}

/// Forget cached return types and parsed scripts (call after files changed).
pub fn clear_cache() {
    if let Ok(mut s) = SUMMARIES.lock() {
        s.clear();
    }
    if let Ok(mut s) = REF_SUMMARIES.lock() {
        s.clear();
    }
    if let Ok(mut s) = FIELDS.lock() {
        s.clear();
    }
    if let Ok(mut s) = WRITES.lock() {
        s.clear();
    }
    super::objids::clear_cache();
    super::callers::clear_cache();
    if let Ok(mut t) = TREES.lock() {
        t.clear();
    }
}

/// The parsed script at `path` (from disk, cached); `current` = (path key, source) of the file being analysed.
pub(crate) fn parsed_script(path: &std::path::Path, current: Option<(&str, &str)>) -> Option<Arc<(String, Tree)>> {
    let key = path_key(path);
    if let Some(t) = TREES.lock().ok().and_then(|m| m.get(&key).cloned()) {
        return Some(t);
    }
    let src = match current {
        Some((k, src)) if k == key => src.to_string(),
        _ => decode_bytes(&std::fs::read(path).ok()?),
    };
    let tree = new_parser().parse(&src, None)?;
    let entry = Arc::new((src, tree));
    if let Ok(mut m) = TREES.lock() {
        m.insert(key, entry.clone());
    }
    Some(entry)
}

const MAX_DEPTH: u8 = 4;

pub struct Infer<'a> {
    index: &'a Index,
    info: &'a FileInfo,
    src: &'a str,
    strict: u8,
    in_global: bool,
    depth: u8,
    locals: HashSet<String>,
    loops: Vec<(Option<State>, Option<State>)>,
    out: FuncTypes,
    /// reference parameters (&x): (index, name)
    refs: Vec<(usize, String)>,
}

/// Analyse function `func` (a function_definition or old_function_definition node of `info`'s source `src`).
pub fn infer_function(index: &Index, info: &FileInfo, src: &str, func: Node, strict: u8) -> FuncTypes {
    infer_at_depth(index, info, src, func, strict, 0)
}

fn infer_at_depth(index: &Index, info: &FileInfo, src: &str, func: Node, strict: u8, depth: u8) -> FuncTypes {
    let in_global = func.child_by_field_name("visibility").map(|v| node_text(&v, src) == "global").unwrap_or(false);
    let mut inf = Infer { index, info, src, strict, in_global, depth, locals: HashSet::new(), loops: vec![], out: FuncTypes::default(), refs: vec![] };
    let mut state = State::new();
    // parameters: typed ones have their type (int/bool convert nil to 0/false, the others may be nil)
    if let Some(pl) = named_children(&func).into_iter().find(|c| c.kind() == "parameter_list") {
        for (i, p) in named_children(&pl).into_iter().filter(|c| c.kind() == "param").enumerate() {
            let Some(name) = p.child_by_field_name("name") else { continue };
            inf.out.ref_out.push((NONE, false));
            if super::index::is_ref_param(&p) {
                let n = node_text(&name, src).to_string();
                state.insert(ref_untouched(&n), INT);
                inf.refs.push((i, n));
            }
            let t = match p.child_by_field_name("type").filter(|_| !super::index::is_ref_param(&p)) {
                Some(ty) => {
                    let t = from_name(node_text(&ty, src));
                    if t == ANY || t & (INT | BOOL) != 0 { t } else { t | NIL }
                }
                None => ANY,
            };
            let n = node_text(&name, src).to_string();
            inf.locals.insert(n.clone());
            state.insert(n, t);
            inf.out.at.insert(name.id(), t);
        }
    }
    // what the callers guarantee (callers.rs): parameter types, call facts at the start
    if super::diagnostics::type_checks_enabled() {
        let fname = func.child_by_field_name("name");
        let row = fname.map(|x| x.start_position().row as u32);
        // only where it can matter: the body calls a nil-capable function or uses a parameter with ->
        let body = node_text(&func, src);
        let useful = |d: &FuncDef| NIL_SOURCES.iter().any(|f| body.contains(&format!("{}(", f)))
            || d.params.iter().any(|(_, p)| body.contains(&format!("{}->", p)) || body.contains(&format!("{} ->", p)));
        if let Some(fdef) = info.funcs.iter().find(|d| Some(d.name_range.start.line) == row && fname.map(|x| node_text(&x, src) == d.name).unwrap_or(false) && useful(d)) {
            // while callers are analysed (one level): only the engine's own guarantees
            let cf = if super::callers::active() { super::callers::facts(index, info, src, fdef) } else { super::callers::engine_facts(index, info, fdef) };
            if let Some(pl) = named_children(&func).into_iter().find(|c| c.kind() == "parameter_list") {
                for (i, p) in named_children(&pl).into_iter().filter(|c| c.kind() == "param").enumerate() {
                    let (Some(name), Some(Some(u))) = (p.child_by_field_name("name"), cf.params.get(i)) else { continue };
                    if super::index::is_ref_param(&p) {
                        continue;
                    }
                    let n = node_text(&name, src).to_string();
                    let declared = state.get(&n).copied().unwrap_or(ANY);
                    // a typed int/bool parameter converts what it gets; others hold exactly the arguments
                    if declared & (INT | BOOL) != 0 && declared != ANY {
                        continue;
                    }
                    let t = if declared & *u == NONE { declared } else { declared & *u };
                    state.insert(n, t);
                    inf.out.at.insert(name.id(), t);
                }
            }
            for k in cf.facts {
                state.insert(k, !NIL);
            }
        }
    }
    // var declarations anywhere in the function are function-scoped and nil until assigned
    let mut stack = vec![func];
    while let Some(n) = stack.pop() {
        if n.kind() == "var_assignment" {
            let scope = n.parent().and_then(|p| named_children(&p).into_iter().find(|c| c.kind() == "var_scope"));
            if scope.map(|s| node_text(&s, src) == "var").unwrap_or(false) {
                if let Some(id) = named_children(&n).first() {
                    let name = node_text(id, src).to_string();
                    inf.locals.insert(name.clone());
                    state.entry(name).or_insert(NIL);
                }
            }
        }
        if n.kind() == "for_statement" {
            // for (var x in a) / for (var i, x in a)
            if children(&n).iter().any(|c| c.kind() == "in") && children(&n).iter().any(|c| c.kind() == "var") {
                for id in super::index::for_in_vars(&n) {
                    let name = node_text(&id, src).to_string();
                    inf.locals.insert(name.clone());
                    state.entry(name).or_insert(NIL);
                }
            }
        }
        stack.extend(named_children(&n));
    }
    let body: Vec<Node> = match func.child_by_field_name("block") {
        Some(b) => vec![b],
        None => named_children(&func).into_iter().filter(|c| !matches!(c.kind(), "visibility" | "identifier" | "comment")).collect(),
    };
    let mut s = Some(state);
    for b in body {
        s = inf.stmt(b, s);
    }
    if s.is_some() {
        inf.out.returns |= NIL; // falls off the end: nil
    }
    inf.exit(&s);
    inf.out
}

impl<'a> Infer<'a> {
    /// Set a variable's type on this path (and the shadow entries of a reference parameter).
    fn put(&self, st: &mut State, name: &str, t: Ty) {
        st.insert(name.to_string(), t);
        // a call fact about GetCrew(iPlr) ends when iPlr changes
        if st.keys().any(|k| k.starts_with(CALL_FACT)) {
            let is_arg = |k: &str| k.split(|c| c == '(' || c == ',' || c == ')').skip(1).any(|a| a == name);
            st.retain(|k, _| !k.starts_with(CALL_FACT) || !is_arg(k));
        }
        if self.refs.iter().any(|(_, r)| r == name) {
            st.insert(ref_written(name), t);
            st.insert(ref_untouched(name), NONE);
        }
    }

    /// A path leaves the function: what it wrote into the reference parameters.
    fn exit(&mut self, s: &Option<State>) {
        let Some(st) = s else { return };
        for (i, name) in &self.refs {
            if let Some(o) = self.out.ref_out.get_mut(*i) {
                o.0 |= st.get(&ref_written(name)).copied().unwrap_or(NONE);
                o.1 |= st.get(&ref_untouched(name)).copied().unwrap_or(NONE) != NONE;
            }
        }
    }

    fn text(&self, n: &Node) -> &'a str {
        node_text(n, self.src)
    }

    fn stmt(&mut self, n: Node, s: Option<State>) -> Option<State> {
        s.as_ref()?;
        match n.kind() {
            "block" => {
                let mut s = s;
                for c in named_children(&n) {
                    s = self.stmt(c, s);
                }
                s
            }
            "var_definition" | "var_definition_inline" => {
                let mut s = s;
                let is_var = named_children(&n).iter().any(|c| c.kind() == "var_scope" && self.text(c) == "var");
                for va in named_children(&n).into_iter().filter(|c| c.kind() == "var_assignment") {
                    let nc = named_children(&va);
                    let Some(id) = nc.first() else { continue };
                    let t = match nc.get(1) {
                        Some(e) => self.expr(*e, &mut s),
                        None => NIL,
                    };
                    if is_var {
                        if let Some(st) = s.as_mut() {
                            self.put(st, self.text(id), t);
                        }
                        self.out.at.insert(id.id(), t);
                    }
                }
                s
            }
            "if_statement" => {
                let nc: Vec<Node> = named_children(&n).into_iter().filter(|c| c.kind() != "comment").collect();
                let mut s = s;
                let Some(cond) = nc.first() else { return s };
                self.expr(*cond, &mut s);
                let else_at = nc.iter().position(|c| c.kind() == "else");
                let then = nc.get(1).filter(|c| c.kind() != "else");
                let (st, sf) = (self.narrow(cond, &s, true), self.narrow(cond, &s, false));
                let a = match then {
                    Some(t) => self.stmt(*t, st),
                    None => st,
                };
                let b = match else_at.and_then(|i| nc.get(i + 1)) {
                    Some(e) => self.stmt(*e, sf),
                    None => sf,
                };
                merge(a, b)
            }
            "while_statement" => {
                let nc: Vec<Node> = named_children(&n).into_iter().filter(|c| c.kind() != "comment").collect();
                self.run_loop(nc.first().copied(), nc.get(1).copied(), None, s)
            }
            "for_statement" => self.for_stmt(n, s),
            "return_statement" => {
                let mut s = s;
                let nc: Vec<Node> = named_children(&n).into_iter().filter(|c| c.kind() != "comment").collect();
                let t = match nc.first() {
                    Some(e) => self.expr(*e, &mut s),
                    None => NIL,
                };
                self.out.returns |= t;
                self.exit(&s);
                None
            }
            "flow_control_statement" => {
                let brk = self.text(&n).starts_with("break");
                if let Some(l) = self.loops.last_mut() {
                    if brk {
                        l.0 = merge(l.0.take(), s);
                    } else {
                        l.1 = merge(l.1.take(), s);
                    }
                }
                None
            }
            "func_desc" | "comment" => s,
            _ => {
                let mut s = s;
                self.expr(n, &mut s);
                s
            }
        }
    }

    fn for_stmt(&mut self, n: Node, s: Option<State>) -> Option<State> {
        let all = children(&n);
        let body = all.iter().rev().find(|c| c.is_named() && c.kind() != "comment").copied();
        let mut s = s;
        if all.iter().any(|c| c.kind() == "in") {
            // for ([var] x in a) / for ([var] i, x in a): the collection, then the loop variables are unknown
            let named: Vec<Node> = all.iter().filter(|c| c.is_named() && c.kind() != "comment").copied().collect();
            let in_pos = all.iter().position(|c| c.kind() == "in").unwrap_or(0);
            let tc = match all.iter().skip(in_pos + 1).find(|c| c.is_named()) {
                Some(coll) => self.expr(*coll, &mut s),
                None => ANY,
            };
            let vars: Vec<Node> = all.iter().take(in_pos).filter(|c| c.kind() == "identifier").copied().collect();
            // one variable: the array's elements (C4AulExec AB_FOREACH_NEXT); two: key and value of a map
            let elem = if vars.len() == 1 && kinds(tc) & !NIL == ARRAY { elements(tc) } else { ANY };
            for v in &vars {
                if let Some(st) = s.as_mut() {
                    if self.locals.contains(self.text(v)) {
                        self.put(st, self.text(v), elem);
                    }
                }
                self.out.at.insert(v.id(), elem);
            }
            let _ = named;
            return self.run_loop(None, body, None, s);
        }
        // for (init; cond; update) body: the parts between the ';' tokens
        let open = all.iter().position(|c| c.kind() == "(").unwrap_or(0);
        let close = all.iter().rposition(|c| c.kind() == ")").unwrap_or(all.len());
        let mut parts: Vec<Option<Node>> = vec![None];
        for c in &all[open + 1..close] {
            if c.kind() == ";" {
                parts.push(None);
            } else if c.is_named() && c.kind() != "comment" {
                *parts.last_mut().unwrap() = Some(*c);
            }
        }
        if let Some(Some(init)) = parts.first() {
            s = self.stmt(*init, s);
        }
        let cond = parts.get(1).copied().flatten();
        let update = parts.get(2).copied().flatten();
        self.run_loop(cond, body, update, s)
    }

    /// while/for: repeat until the state at the loop start stops growing; the loop ends when the condition is
    /// false (state after the condition) or at a break.
    fn run_loop(&mut self, cond: Option<Node>, body: Option<Node>, update: Option<Node>, s: Option<State>) -> Option<State> {
        let mut entry = s.clone();
        let mut exit = None;
        for _ in 0..8 {
            let mut after_cond = entry.clone();
            if let Some(c) = cond {
                self.expr(c, &mut after_cond);
            }
            self.loops.push((None, None));
            let (into, out_of) = match cond {
                Some(c) => (self.narrow(&c, &after_cond, true), self.narrow(&c, &after_cond, false)),
                None => (after_cond.clone(), after_cond.clone()),
            };
            let out = match body {
                Some(b) => self.stmt(b, into),
                None => into,
            };
            let (brk, cont) = self.loops.pop().unwrap_or((None, None));
            let mut end = merge(out, cont);
            if let Some(u) = update {
                self.expr(u, &mut end);
            }
            exit = merge(out_of, brk);
            let next = merge(s.clone(), end);
            if next == entry {
                break;
            }
            entry = next;
        }
        exit
    }

    /// The state where `cond` is `truth`: a variable that is true there isn't nil (`x`, `x != nil`, `x = F()`, `!`,
    /// `&&`, `||`, `x == nil` being false).
    fn narrow(&self, cond: &Node, s: &Option<State>, truth: bool) -> Option<State> {
        let mut out = s.clone();
        if let Some(st) = out.as_mut() {
            self.narrow_into(cond, st, truth);
        }
        out
    }

    /// The call-fact key of a call whose result a guard can pin: a NIL_SOURCES engine call (not obj->F()) whose
    /// arguments are variables, numbers or `this` (`Contained()`, `GetCrew(iPlr)`, `Contents(0, pObj)`).
    fn call_key(&self, n: &Node) -> Option<String> {
        if n.kind() != "method_call" || is_arrow_call(n) {
            return None;
        }
        let name = self.text(&n.child_by_field_name("name")?);
        if !NIL_SOURCES.contains(&name) {
            return None;
        }
        let args: Vec<Node> = n.child_by_field_name("args").map(|a| named_children(&a)).unwrap_or_default()
            .into_iter().filter(|c| c.kind() != "comment").collect();
        // variables, numbers, IDs and such calls themselves (Contained(GetCrew(i, j)))
        if !args.iter().all(|a| matches!(a.kind(), "identifier" | "number" | "nil" | "id") || self.call_key(a).is_some()) {
            return None;
        }
        let args: Vec<String> = args.iter().map(|a| self.text(a).split_whitespace().collect::<String>()).collect();
        Some(format!("{}{}({})", CALL_FACT, name, args.join(",")))
    }

    /// `i < GetCrewCount(p)` -> the call fact of GetCrew(p,i); `i < ContentsCount()` / `ContentsCount(0, obj)` ->
    /// Contents(i) / Contents(i,obj) (only without an ID filter: ContentsCount(id) counts other things).
    fn count_fact(&self, idx: &Node, count: &Node) -> Option<String> {
        if idx.kind() != "identifier" || count.kind() != "method_call" || is_arrow_call(count) {
            return None;
        }
        let i = self.text(idx);
        let name = self.text(&count.child_by_field_name("name")?);
        let args: Vec<String> = count.child_by_field_name("args").map(|a| named_children(&a)).unwrap_or_default()
            .into_iter().filter(|c| c.kind() != "comment").map(|c| self.text(&c).split_whitespace().collect::<String>()).collect();
        if !args.iter().all(|a| a.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')) {
            return None;
        }
        match name {
            "GetCrewCount" if args.len() == 1 => Some(call_fact(&format!("GetCrew({},{})", args[0], i))),
            "ContentsCount" if args.is_empty() || matches!(args[0].as_str(), "0" | "nil") => match args.get(1) {
                Some(obj) => Some(call_fact(&format!("Contents({},{})", i, obj))),
                None => Some(call_fact(&format!("Contents({})", i))),
            },
            _ => None,
        }
    }

    fn narrow_into(&self, cond: &Node, st: &mut State, truth: bool) {
        let nc: Vec<Node> = named_children(cond).into_iter().filter(|c| c.kind() != "comment").collect();
        let op = children(cond).iter().find(|c| !c.is_named() && !matches!(c.kind(), "(" | ")")).map(|c| c.kind().to_string()).unwrap_or_default();
        let not_nil = |n: &Node, st: &mut State| {
            if n.kind() == "identifier" && self.locals.contains(self.text(n)) {
                if let Some(t) = st.get_mut(self.text(n)) {
                    if *t & NIL != 0 && *t & !NIL != 0 {
                        *t &= !NIL;
                    }
                }
            }
        };
        let fact = |n: &Node, st: &mut State| {
            if let Some(k) = self.call_key(n) {
                st.insert(k, !NIL);
            }
        };
        match (cond.kind(), op.as_str()) {
            ("identifier", _) if truth => not_nil(cond, st),
            ("method_call", _) if truth => fact(cond, st),
            // for (i = 0; i < GetCrewCount(p); i++) GetCrew(p, i): within the count the item exists
            ("binary_expression", "<" | ">") if truth => {
                if let (Some(a), Some(b)) = (nc.first(), nc.get(1)) {
                    let (idx, count) = if op == "<" { (a, b) } else { (b, a) };
                    if let Some(k) = self.count_fact(idx, count) {
                        st.insert(k, !NIL);
                    }
                }
            }
            ("unary_expression", "!") => {
                if let Some(x) = nc.first() {
                    self.narrow_into(x, st, !truth);
                }
            }
            ("binary_expression", "=") if truth => {
                if let Some(x) = nc.first() {
                    not_nil(x, st);
                }
                if let Some(v) = nc.get(1) {
                    fact(v, st);
                }
            }
            ("binary_expression", "&&") if truth => nc.iter().for_each(|x| self.narrow_into(x, st, true)),
            ("binary_expression", "||") if !truth => nc.iter().for_each(|x| self.narrow_into(x, st, false)),
            ("binary_expression", "!=" | "==") if (op == "!=") == truth => {
                if let (Some(a), Some(b)) = (nc.first(), nc.get(1)) {
                    match (a.kind(), b.kind()) {
                        (_, "nil") => { not_nil(a, st); fact(a, st); }
                        ("nil", _) => { not_nil(b, st); fact(b, st); }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn assign(&mut self, lhs: &Node, t: Ty, s: &mut Option<State>) {
        if lhs.kind() != "identifier" {
            return;
        }
        let name = self.text(lhs);
        if self.locals.contains(name) {
            if let Some(st) = s.as_mut() {
                self.put(st, name, t);
            }
            self.out.at.insert(lhs.id(), t);
        } else if name != "this" {
            *self.out.writes.entry(name.to_string()).or_insert(NONE) |= t;
        }
    }

    /// The operator token of a unary/binary expression (parentheses of inlined (...) operands are skipped).
    fn op(&self, n: &Node) -> String {
        children(n).iter().find(|c| !c.is_named() && !matches!(c.kind(), "(" | ")")).map(|c| c.kind().to_string()).unwrap_or_default()
    }

    pub fn expr(&mut self, n: Node, s: &mut Option<State>) -> Ty {
        if s.is_none() {
            return ANY;
        }
        let t = match n.kind() {
            "number" => INT,
            "string" | "translation" => STRING,
            "bool" => BOOL,
            "nil" => NIL,
            "id" => {
                if self.text(&n).chars().all(|c| c.is_ascii_digit()) { INT } else { ID }
            }
            "array" => {
                let mut elem = NONE;
                for c in named_children(&n).into_iter().filter(|c| c.kind() != "comment") {
                    elem |= self.expr(c, s);
                }
                array_of(elem)
            }
            "map" => {
                for e in named_children(&n).into_iter().filter(|c| c.kind() == "map_entry") {
                    if let Some(v) = named_children(&e).into_iter().last() {
                        self.expr(v, s);
                    }
                }
                MAP
            }
            "identifier" => {
                let name = self.text(&n);
                if name == "this" {
                    OBJECT
                } else if self.locals.contains(name) {
                    s.as_ref().and_then(|st| st.get(name).copied()).unwrap_or(ANY)
                } else if let Some(c) = ENGINE.constants.get(name) {
                    from_name(&c.ty)
                } else {
                    self.field(name).map(|f| f.ty).unwrap_or(ANY)
                }
            }
            "map_access" => {
                let nc: Vec<Node> = named_children(&n).into_iter().filter(|c| c.kind() != "comment").collect();
                let dot = children(&n).iter().any(|c| c.kind() == ".");
                let tb = match nc.first() {
                    Some(b) => self.expr(*b, s),
                    None => ANY,
                };
                if !dot {
                    if let Some(i) = nc.get(1) {
                        self.expr(*i, s);
                    }
                }
                // a[i] of an array: an element, or nil out of range
                if !dot && kinds(tb) & !NIL == ARRAY { elements(tb) | NIL } else { ANY }
            }
            "unary_expression" => {
                let op = self.op(&n);
                let operand = named_children(&n).into_iter().find(|c| c.kind() != "comment");
                if let Some(o) = operand {
                    let t = self.expr(o, s);
                    if op == "++" || op == "--" {
                        self.out.read_at.insert(o.id(), t);
                        self.assign(&o, INT, s);
                    }
                }
                if op == "!" { BOOL } else { INT }
            }
            "binary_expression" => self.binary(n, s),
            "method_call" => {
                let args: Vec<Node> = n.child_by_field_name("args").map(|a| named_children(&a)).unwrap_or_default()
                    .into_iter().filter(|c| c.kind() != "comment").collect();
                let tys: Vec<Ty> = args.iter().map(|a| self.expr(*a, s)).collect();
                let name = n.child_by_field_name("name").map(|x| self.text(&x)).unwrap_or("");
                // callers.rs: the call facts a function called here starts with
                if super::diagnostics::caller_inference_enabled() && (!is_arrow_call(&n) || n.parent().and_then(|p| named_children(&p).into_iter().next()).map(|t| self.text(&t) == "this").unwrap_or(false)) {
                    if let Some(st) = s.as_ref() {
                        let facts: Vec<String> = st.keys().filter(|k| k.starts_with(CALL_FACT) && transferable(k)).cloned().collect();
                        self.out.site_facts.insert(n.id(), facts);
                    }
                }
                // Exit(obj)/Enter(target, obj) on another object leave this one's containment alone
                let other = |i: usize| args.get(i).map(|a| !matches!(self.text(a), "this" | "this()" | "nil" | "0")).unwrap_or(false);
                let moves_other = match name {
                    "Exit" => other(0),
                    "Enter" => other(1),
                    _ => false,
                };
                if let (Some(funcs), false, Some(st)) = (fact_breaks(name), moves_other, s.as_mut()) {
                    // an empty list ends every call fact
                    st.retain(|k, _| {
                        let Some(f) = k.strip_prefix(CALL_FACT) else { return true };
                        let called = f.split('(').next().unwrap_or("");
                        !funcs.is_empty() && !funcs.contains(&called)
                    });
                }
                // a variable passed to a reference parameter: a script's &x can hold anything afterwards; engine
                // functions write a known type (engine_api.json "&int": Inc/Dec replace the value, PathFree2/
                // SimFlight write only sometimes; Set writes its second argument)
                // (a script variable gets the written part as one of its assignments: FuncTypes::writes)
                for (i, a) in args.iter().enumerate() {
                    if a.kind() != "identifier" || self.text(a) == "this" {
                        continue;
                    }
                    let engine_ref = ENGINE.functions.get(name).and_then(|f| f.params.as_ref()?.get(i)).map(|p| p.etype.starts_with('&')).unwrap_or(false);
                    let is_ref = |f: &FuncDef| f.name == name && f.params.get(i).and_then(|p| p.0.as_deref()) == Some("&");
                    if !engine_ref && !self.info.funcs.iter().any(is_ref) && !self.index.all_funcs(name).iter().any(|(_, f)| is_ref(f)) {
                        continue;
                    }
                    let reach = if is_arrow_call(&n) { self.index.reachable_funcs(&self.info.path, name) } else { self.index.visible_funcs(self.info, name) };
                    // (type written, whether the old value can stay)
                    let written: Option<(Ty, bool)> = if !reach.is_empty() {
                        let refs: Vec<_> = reach.into_iter().filter(|(_, f)| f.params.get(i).and_then(|p| p.0.as_deref()) == Some("&")).collect();
                        // what the function body leaves in it (the caller's value where some path doesn't write)
                        (!refs.is_empty()).then(|| refs.iter().fold((NONE, false), |(t, k), (file, f)| {
                            let (w, keep) = self.ref_summary(file, f, i);
                            (t | w, k || keep)
                        }))
                    } else {
                        let et = ENGINE.functions.get(name).and_then(|f| f.params.as_ref()?.get(i)).map(|p| p.etype.as_str()).unwrap_or("");
                        match et.strip_prefix('&') {
                            Some(_) if name == "Set" => Some((tys.get(1).copied().unwrap_or(ANY), false)),
                            Some(t) => Some((from_name(t), !matches!(name, "Inc" | "Dec"))),
                            None => None,
                        }
                    };
                    let Some((w, keep)) = written else { continue };
                    let var = self.text(a);
                    if self.locals.contains(var) {
                        let old = s.as_ref().and_then(|st| st.get(var).copied()).unwrap_or(ANY);
                        if let Some(st) = s.as_mut() {
                            self.put(st, var, if keep { w | old } else { w });
                        }
                    } else if w != NONE {
                        *self.out.writes.entry(var.to_string()).or_insert(NONE) |= w;
                    }
                }
                // obj->F(): the receiver's definitions when known (objids.rs)
                let ids = if is_arrow_call(&n) { super::objids::receiver_ids(self.index, self.info, self.src, &n) } else { None };
                let t = self.call_type(name, is_arrow_call(&n) || n.child_by_field_name("id").is_some(), ids);
                // a guarded call (if (!Contained()) return; ... Contained()) isn't nil
                match self.call_key(&n).and_then(|k| s.as_ref()?.get(&k).copied()) {
                    Some(f) => t & f,
                    None => t,
                }
            }
            "arrow_expression" => {
                let nc: Vec<Node> = named_children(&n).into_iter().filter(|c| c.kind() != "comment").collect();
                if let Some(target) = nc.first() {
                    self.expr(*target, s);
                }
                let t = match nc.last() {
                    Some(call) if call.kind() == "method_call" => self.expr(*call, s),
                    _ => ANY,
                };
                let safe = children(&n).iter().any(|c| c.kind() == "?");
                // x->F() ran: x wasn't nil (nil->F() stops the script), so it isn't nil afterwards either
                if let (false, Some(target), Some(st)) = (safe, nc.first(), s.as_mut()) {
                    self.narrow_into(target, st, true);
                }
                if safe { t | NIL } else { t }
            }
            _ => {
                for c in named_children(&n) {
                    self.expr(c, s);
                }
                ANY
            }
        };
        self.out.at.insert(n.id(), t);
        t
    }

    fn binary(&mut self, n: Node, s: &mut Option<State>) -> Ty {
        let nc: Vec<Node> = named_children(&n).into_iter().filter(|c| c.kind() != "comment").collect();
        let (Some(l), Some(r)) = (nc.first().copied(), nc.last().copied()) else { return ANY };
        let op = self.op(&n);
        match op.as_str() {
            "=" => {
                let tr = self.expr(r, s);
                match l.kind() {
                    "identifier" => self.assign(&l, tr, s),
                    // a[] = x, a[i] = x: the array also holds x now
                    "array_append" | "map_access" if !children(&l).iter().any(|c| c.kind() == ".") => {
                        self.expr(l, s);
                        if let Some(base) = named_children(&l).into_iter().next().filter(|b| b.kind() == "identifier") {
                            let cur = self.expr(base, s);
                            if kinds(cur) & ARRAY != 0 {
                                self.assign(&base, cur | array_of(tr), s);
                            } else if !self.locals.contains(self.text(&base)) {
                                self.assign(&base, array_of(tr), s);
                            }
                        }
                    }
                    // obj.name = x: a local of another object (not a map key)
                    "map_access" => {
                        self.expr(l, s);
                        let nc = named_children(&l);
                        let tb = nc.first().and_then(|b| self.out.at.get(&b.id()).copied()).unwrap_or(ANY);
                        if let (Some(key), true) = (nc.get(1), tb & OBJECT != 0) {
                            let def = nc.first().and_then(|b| self.receiver_def(b));
                            self.out.member_writes.entry(self.text(key).to_string()).or_default().push((def, tr));
                        }
                    }
                    // LocalN("name"[, obj]) = x
                    "method_call" => {
                        self.expr(l, s);
                        let name = l.child_by_field_name("name").map(|x| self.text(&x)).unwrap_or("");
                        let args: Vec<Node> = l.child_by_field_name("args").map(|a| named_children(&a)).unwrap_or_default()
                            .into_iter().filter(|c| c.kind() != "comment").collect();
                        if let (true, Some(f)) = (name == "LocalN", args.first().filter(|f| f.kind() == "string")) {
                            // the object: the second argument, else this object
                            let def = match args.get(1) {
                                Some(o) => self.receiver_def(o),
                                None => self.info.def_id.clone(),
                            };
                            self.out.member_writes.entry(self.text(f).trim_matches('"').to_string()).or_default().push((def, tr));
                        }
                    }
                    _ => {
                        self.expr(l, s);
                    }
                }
                tr
            }
            "+=" | "-=" | "*=" | "/=" | "%=" | "**=" | "<<=" | ">>=" | "&=" | "|=" | "^=" => {
                let tl = self.expr(l, s);
                self.out.read_at.insert(l.id(), tl);
                self.expr(r, s);
                self.assign(&l, INT, s);
                INT
            }
            "..=" => {
                let tl = self.expr(l, s);
                self.out.read_at.insert(l.id(), tl);
                let tr = self.expr(r, s);
                let t = concat(tl, tr);
                self.assign(&l, t, s);
                t
            }
            "??=" => {
                let tl = self.expr(l, s);
                let mut s2 = s.clone();
                let tr = self.expr(r, &mut s2);
                let t = if tl & NIL != 0 { (tl & !NIL) | tr } else { tl };
                *s = merge(s.take(), s2);
                self.assign(&l, t, s);
                t
            }
            "&&" | "||" => {
                let tl = self.expr(l, s);
                // the right side runs only when the left one is true (&&) / false (||), from #strict 2 on
                let mut s2 = if self.strict >= 2 { self.narrow(&l, s, op == "&&") } else { s.clone() };
                let tr = self.expr(r, &mut s2); // only evaluated sometimes
                *s = merge(s.take(), s2);
                if self.strict >= 2 { tl | tr } else { BOOL }
            }
            "??" => {
                let tl = self.expr(l, s);
                let mut s2 = s.clone();
                let tr = self.expr(r, &mut s2);
                *s = merge(s.take(), s2);
                if tl & NIL != 0 { (tl & !NIL) | tr } else { tl }
            }
            ".." => {
                let tl = self.expr(l, s);
                let tr = self.expr(r, s);
                concat(tl, tr)
            }
            "==" | "!=" | "S=" | "eq" | "ne" | "<" | ">" | "<=" | ">=" => {
                self.expr(l, s);
                self.expr(r, s);
                BOOL
            }
            _ => {
                self.expr(l, s);
                self.expr(r, s);
                INT
            }
        }
    }

    /// Type of a call by name: script functions (their inferred return types), else the engine function.
    fn call_type(&self, name: &str, arrow: bool, ids: Option<Vec<String>>) -> Ty {
        match name {
            "this" => return OBJECT,
            "Par" | "inherited" | "_inherited" => return ANY,
            _ => {}
        }
        if let Some(ids) = ids {
            // the object's own functions win over the engine's (C4AulFunc::FindSameNameFunc)
            let funcs = super::objids::member_funcs(self.index, self.info, &ids, name);
            if funcs.is_empty() {
                return engine_return(name).unwrap_or(ANY);
            }
            let t = funcs.iter().fold(NONE, |t, (file, f)| t | self.summary(file, f));
            return if t == NONE { ANY } else { t };
        }
        let funcs = if arrow {
            self.index.reachable_funcs(&self.info.path, name)
        } else if self.in_global {
            self.index.global_funcs(&self.info.path, name)
        } else {
            self.index.visible_funcs(self.info, name)
        };
        if funcs.is_empty() {
            return engine_return(name).unwrap_or(ANY);
        }
        if funcs.len() > 6 {
            return ANY;
        }
        let mut t = NONE;
        for (file, f) in funcs {
            t |= self.summary(file, f);
        }
        if arrow {
            if let Some(e) = engine_return(name) {
                t |= e;
            }
        }
        if t == NONE { ANY } else { t }
    }

    /// What a script function leaves in its reference parameter `i`: (type written, some path doesn't write);
    /// (ANY, true) when unknown (recursion, depth).
    fn ref_summary(&self, file: &FileInfo, f: &FuncDef, i: usize) -> (Ty, bool) {
        if self.depth >= MAX_DEPTH {
            return (ANY, true);
        }
        let key = format!("{}|{}|{}", path_key(&file.path), f.name, f.name_range.start.line);
        if let Some(r) = REF_SUMMARIES.lock().ok().and_then(|m| m.get(&key).cloned()) {
            return r.get(i).copied().unwrap_or((ANY, true));
        }
        if !IN_PROGRESS.lock().map(|mut p| p.insert(busy_key(&format!("ref|{}", key)))).unwrap_or(false) {
            return (ANY, true);
        }
        let current_key = path_key(&self.info.path);
        let r: Vec<(Ty, bool)> = parsed_script(&file.path, Some((&current_key, self.src))).and_then(|entry| {
            let (src, tree) = (&entry.0, &entry.1);
            let root = tree.root_node();
            let strict = super::diagnostics::strict_level(&root, src);
            let func = find_func(&root, src, f)?;
            Some(infer_at_depth(self.index, file, src, func, strict, self.depth + 1).ref_out)
        }).unwrap_or_default();
        if let Ok(mut p) = IN_PROGRESS.lock() {
            p.remove(&busy_key(&format!("ref|{}", key)));
        }
        if let Ok(mut m) = REF_SUMMARIES.lock() {
            m.insert(key, r.clone());
        }
        r.get(i).copied().unwrap_or((ANY, true))
    }

    /// Inferred return type of a script function (cached; recursion and deep chains give `any`).
    fn summary(&self, file: &FileInfo, f: &FuncDef) -> Ty {
        if self.depth >= MAX_DEPTH {
            return ANY;
        }
        let key = format!("{}|{}|{}", path_key(&file.path), f.name, f.name_range.start.line);
        if let Some(t) = SUMMARIES.lock().ok().and_then(|m| m.get(&key).copied()) {
            return t;
        }
        {
            let Ok(mut p) = IN_PROGRESS.lock() else { return ANY };
            if !p.insert(busy_key(&key)) {
                return ANY; // recursion
            }
        }
        let current_key = path_key(&self.info.path);
        let t = match parsed_script(&file.path, Some((&current_key, self.src))) {
            Some(entry) => {
                let (src, tree) = (&entry.0, &entry.1);
                let root = tree.root_node();
                let strict = super::diagnostics::strict_level(&root, src);
                match find_func(&root, src, f) {
                    Some(func) => {
                        let r = infer_at_depth(self.index, file, src, func, strict, self.depth + 1).returns;
                        // a 0/false returned by a #strict 2 (or older) script arrives as nil in #strict 3
                        if strict < 3 && r & (INT | BOOL) != 0 { r | NIL } else { r }
                    }
                    None => ANY,
                }
            }
            None => ANY,
        };
        if let Ok(mut p) = IN_PROGRESS.lock() {
            p.remove(&busy_key(&key));
        }
        if let Ok(mut m) = SUMMARIES.lock() {
            m.insert(key, t);
        }
        t
    }
}

impl<'a> Infer<'a> {
    /// Definition of the object an expression gives, when it shows: `this`, an ID, Create*/Find*/Place*(ID ...)
    /// (also through Find_ID(ID)).
    fn receiver_def(&self, n: &Node) -> Option<String> {
        match n.kind() {
            "identifier" if self.text(n) == "this" => self.info.def_id.clone(),
            "id" => Some(self.text(n).to_string()),
            "method_call" => {
                let name = n.child_by_field_name("name").map(|x| self.text(&x)).unwrap_or("");
                if !["Create", "Find", "Place", "Compose"].iter().any(|p| name.starts_with(p)) {
                    return None;
                }
                let mut stack = n.child_by_field_name("args").map(|a| named_children(&a)).unwrap_or_default();
                while let Some(c) = stack.pop() {
                    if c.kind() == "id" && !self.text(&c).chars().all(|ch| ch.is_ascii_digit()) {
                        return Some(self.text(&c).to_string());
                    }
                    if c.kind() == "method_call" && c.child_by_field_name("name").map(|x| self.text(&x) == "Find_ID").unwrap_or(false) {
                        stack.extend(c.child_by_field_name("args").map(|a| named_children(&a)).unwrap_or_default());
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// The open document's fresh info replaces its index entry.
    fn fresh<'b>(&'b self, f: &'b FileInfo) -> &'b FileInfo {
        if path_key(&f.path) == path_key(&self.info.path) { self.info } else { f }
    }

    /// Analyse function number `i` of `file` (from disk, or the open document's text).
    /// The script-variable writes of function `i` of `file` (cached: a function writing several fields is
    /// inferred once, not once per field).
    fn writes_of(&self, file: &FileInfo, i: usize) -> Option<Arc<Writes>> {
        let f = file.funcs.get(i)?;
        let key = format!("{}|{}|{}|{}", path_key(&file.path), f.name, f.name_range.start.line, self.depth);
        if let Some(w) = WRITES.lock().ok().and_then(|m| m.get(&key).cloned()) {
            return Some(w);
        }
        let ft = self.infer_other(file, i)?;
        let w = Arc::new(Writes { writes: ft.writes, member_writes: ft.member_writes });
        if let Ok(mut m) = WRITES.lock() {
            m.insert(key, w.clone());
        }
        Some(w)
    }

    fn infer_other(&self, file: &FileInfo, i: usize) -> Option<FuncTypes> {
        let f = file.funcs.get(i)?;
        let current_key = path_key(&self.info.path);
        let entry = parsed_script(&file.path, Some((&current_key, self.src)))?;
        let (src, tree) = (&entry.0, &entry.1);
        let root = tree.root_node();
        let func = find_func(&root, src, f)?;
        let strict = super::diagnostics::strict_level(&root, src);
        Some(infer_at_depth(self.index, file, src, func, strict, self.depth + 1))
    }

    /// Type of the script variable `name` visible here: the union of all values assigned to it (cached).
    fn field(&self, name: &str) -> Option<FieldType> {
        if !super::diagnostics::field_types_enabled() {
            return None;
        }
        let vars = self.index.visible_vars(self.info, name);
        let (decl_file, var) = vars.first()?;
        if var.kind == super::index::VarKind::StaticConst {
            let ty = var.value.as_deref().map(literal_type).unwrap_or(ANY);
            return Some(FieldType { ty, writers: 0, scripts: 0 });
        }
        if self.depth >= MAX_DEPTH {
            return None;
        }
        let is_local = var.kind == super::index::VarKind::Local;
        let key = if is_local { format!("local|{}|{}", path_key(&decl_file.path), name) } else { format!("static|{}", name) };
        if let Some(t) = FIELDS.lock().ok().and_then(|m| m.get(&key).copied()) {
            return Some(t);
        }
        {
            let Ok(mut p) = IN_PROGRESS.lock() else { return None };
            if !p.insert(busy_key(&key)) {
                // assigned from its own value (x = x .. [y]): the other assignments decide
                return Some(FieldType { ty: NONE, writers: 0, scripts: 0 });
            }
        }
        // the functions assigning it: a local from its definition's script family, a static from every script
        // where the name means this static (not a local of its own)
        let family_files: Vec<&FileInfo> = if is_local { self.index.family(self.fresh(decl_file)) } else { vec![] };
        let family: Option<HashSet<String>> = is_local.then(|| {
            family_files.iter().map(|f| path_key(&f.path)).chain(std::iter::once(path_key(&decl_file.path))).collect()
        });
        // obj.name = x counts unless obj is known to be of a definition outside the family
        let family_ids: HashSet<&str> = family_files.iter().filter_map(|f| f.def_id.as_deref()).collect();
        let mut ty = NONE;
        let (mut writers, mut scripts) = (0, 0);
        let current = path_key(&self.info.path);
        let mut sites: Vec<(&FileInfo, usize, bool)> = vec![];
        for (f, i, member) in self.index.writers(name) {
            if path_key(&f.path) != current {
                sites.push((f, i, member));
            }
        }
        for (i, member) in self.info.writes.iter().filter(|(n, _, _)| n == name).map(|(_, i, m)| (*i, *m)) {
            sites.push((self.info, i, member));
        }
        // passed to a reference parameter (Index::ref_writes; the lookup has them for the other files)
        for i in self.index.ref_writes(self.info, name) {
            if !sites.iter().any(|(f, j, m)| path_key(&f.path) == current && *j == i && !m) {
                sites.push((self.info, i, false));
            }
        }
        let mut seen_files = HashSet::new();
        for (f, i, member) in sites {
            let fk = path_key(&f.path);
            if !member {
                match &family {
                    Some(fam) if !fam.contains(&fk) => continue,
                    None => {
                        let own = self.index.visible_vars(f, name);
                        if own.first().map(|(_, v)| v.kind != super::index::VarKind::Static).unwrap_or(true) {
                            continue;
                        }
                    }
                    _ => {}
                }
            } else if !is_local {
                continue;
            }
            let Some(ft) = self.writes_of(f, i) else { continue };
            let t = if member {
                ft.member_writes.get(name).into_iter().flatten()
                    .filter(|(def, _)| def.as_deref().map_or(true, |d| family_ids.contains(d)))
                    .fold(None, |acc: Option<Ty>, (_, t)| Some(acc.unwrap_or(NONE) | *t))
            } else {
                ft.writes.get(name).copied()
            };
            if let Some(t) = t {
                ty |= t;
                writers += 1;
                if seen_files.insert(fk) {
                    scripts += 1;
                }
            }
        }
        if let Ok(mut p) = IN_PROGRESS.lock() {
            p.remove(&busy_key(&key));
        }
        let r = FieldType { ty: if writers == 0 { NIL } else { ty }, writers, scripts };
        if let Ok(mut m) = FIELDS.lock() {
            m.insert(key, r);
        }
        Some(r)
    }
}

/// Inferred return type of script function `f` of `file` (cached), e.g. for hover.
pub fn script_return(index: &Index, file: &FileInfo, f: &FuncDef) -> Ty {
    let inf = Infer { index, info: file, src: "", strict: 3, in_global: false, depth: 0, locals: HashSet::new(), loops: vec![], out: FuncTypes::default(), refs: vec![] };
    inf.summary(file, f)
}

/// Type of the expression/variable use at `node` inside its function (hover).
pub fn type_at(index: &Index, info: &FileInfo, src: &str, root: &Node, node: &Node) -> Option<Ty> {
    let mut f = node.parent();
    while let Some(c) = f {
        if matches!(c.kind(), "function_definition" | "old_function_definition") {
            break;
        }
        f = c.parent();
    }
    let func = f?;
    let strict = super::diagnostics::strict_level(root, src);
    infer_function(index, info, src, func, strict).at.get(&node.id()).copied()
}

fn concat(tl: Ty, tr: Ty) -> Ty {
    let array_or_none = |t: Ty| t == NONE || kinds(t) == ARRAY;
    if array_or_none(tl) && array_or_none(tr) && (tl | tr) != NONE {
        tl | tr // both arrays: the elements of both
    } else if (tl | tr) & ARRAY != 0 {
        ARRAY_ANY | STRING
    } else {
        STRING
    }
}

/// Type of a `static const` from its value's text.
fn literal_type(v: &str) -> Ty {
    let v = v.trim();
    if v.starts_with('"') {
        STRING
    } else if v == "true" || v == "false" {
        BOOL
    } else if v == "nil" {
        NIL
    } else if v.trim_start_matches('-').chars().all(|c| c.is_ascii_digit()) && !v.is_empty() {
        INT
    } else if v.len() == 4 && v.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') {
        ID
    } else {
        ANY
    }
}

/// The function node of `f` in a parsed script.
pub(crate) fn find_func<'t>(root: &Node<'t>, src: &str, f: &FuncDef) -> Option<Node<'t>> {
    named_children(root).into_iter().find(|c| {
        matches!(c.kind(), "function_definition" | "old_function_definition")
            && c.child_by_field_name("name").map(|n| node_text(&n, src) == f.name && n.start_position().row as u32 == f.name_range.start.line).unwrap_or(false)
    })
}

/// Inferred type of the script variable `name` as seen from `info` (None: not a script variable, or switched off).
pub fn field_type(index: &Index, info: &FileInfo, src: &str, name: &str) -> Option<FieldType> {
    let inf = Infer { index, info, src, strict: 3, in_global: false, depth: 0, locals: HashSet::new(), loops: vec![], out: FuncTypes::default(), refs: vec![] };
    inf.field(name)
}
