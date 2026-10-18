//! Which definitions (IDs) an object value can be of.
//!
//! Flow-insensitive and syntactic: a variable's IDs are the union over every value assigned to it in its function
//! (a `local`: in its definition family, like types.rs field_type), read off the assigned expression:
//! `CreateObject(ID, ...)` / `CreateContents` / `CreateConstruction` / `PlaceAnimal` / `PlaceVegetation` /
//! `FindObject(ID, ...)` / `FindContents(ID)` -> {ID}; `FindObject2`/`FindObjects`/`FindObject` criteria with
//! `Find_ID(ID)` (Find_Or -> union, Find_And / several criteria -> intersection); `this` -> the script's definition;
//! an ID value (definition calls); another variable; `a ?? b`, `a || b`, `[a, b]`, `a[i]` (an array's elements);
//! nil/numbers/strings add nothing. Anything else (parameters, other calls, obj.local, `Contents()`) is unknown.
//! A union can only be larger than the truth, so the result is safe for narrowing `->` calls; unknown keeps the
//! old behaviour (any script defining the name). Switch: diagnostics::object_ids_enabled().

use super::index::{for_in_vars, named_children, path_key, FileInfo, FuncDef, Index, VarKind};
use super::text::node_text;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Mutex;
use tree_sitter::Node;

/// None = unknown; Some(set) = the object is of one of these definitions (empty: not an object, e.g. nil)
pub type Ids = Option<BTreeSet<String>>;

lazy_static::lazy_static! {
    /// "local|<decl script>|name" / "static|name" -> IDs (cleared with types::clear_cache)
    static ref FIELD_IDS: Mutex<HashMap<String, Ids>> = Mutex::new(HashMap::new());
    static ref FIELD_IN_PROGRESS: Mutex<HashSet<String>> = Mutex::new(HashSet::new());
}

pub fn clear_cache() {
    if let Ok(mut m) = FIELD_IDS.lock() {
        m.clear();
    }
}

fn union(a: Ids, b: Ids) -> Ids {
    let (mut a, b) = (a?, b?);
    a.extend(b);
    Some(a)
}

fn one(id: &str) -> Ids {
    Some(std::iter::once(id.to_string()).collect())
}

/// Functions whose first argument (a literal ID) is the definition of the object they return.
const CREATORS: &[&str] = &["CreateObject", "CreateContents", "CreateConstruction", "PlaceAnimal", "PlaceVegetation", "FindObject", "FindContents", "FindObjectOwner"];
/// Functions taking Find_* criteria.
const SEARCHES: &[&str] = &["FindObject2", "FindObjects", "FindObject"];

pub struct IdCtx<'a, 't> {
    pub index: &'a Index,
    pub info: &'a FileInfo,
    pub src: &'a str,
    func: Option<Node<'t>>,
    params: HashSet<String>,
    locals: HashSet<String>,
    cache: RefCell<HashMap<String, Ids>>,
    in_progress: RefCell<HashSet<String>>,
    depth: u8,
}

/// The function definition around `n`.
pub fn enclosing_function<'t>(n: &Node<'t>) -> Option<Node<'t>> {
    let mut cur = Some(*n);
    while let Some(c) = cur {
        if matches!(c.kind(), "function_definition" | "old_function_definition") {
            return Some(c);
        }
        cur = c.parent();
    }
    None
}

impl<'a, 't> IdCtx<'a, 't> {
    pub fn new(index: &'a Index, info: &'a FileInfo, src: &'a str, func: Option<Node<'t>>) -> Self {
        Self::at_depth(index, info, src, func, 0)
    }

    fn at_depth(index: &'a Index, info: &'a FileInfo, src: &'a str, func: Option<Node<'t>>, depth: u8) -> Self {
        let (mut params, mut locals) = (HashSet::new(), HashSet::new());
        if let Some(f) = func {
            let mut stack = vec![f];
            while let Some(n) = stack.pop() {
                match n.kind() {
                    "param" => params.extend(n.child_by_field_name("name").map(|x| node_text(&x, src).to_string())),
                    "var_assignment" => {
                        let is_var = n.parent().map(|p| named_children(&p).iter().any(|c| c.kind() == "var_scope" && node_text(c, src) == "var")).unwrap_or(false);
                        if is_var {
                            locals.extend(named_children(&n).first().map(|x| node_text(x, src).to_string()));
                        }
                    }
                    "for_statement" => locals.extend(for_in_vars(&n).iter().map(|x| node_text(x, src).to_string())),
                    _ => {}
                }
                stack.extend(named_children(&n));
            }
        }
        IdCtx { index, info, src, func, params, locals, cache: RefCell::new(HashMap::new()), in_progress: RefCell::new(HashSet::new()), depth }
    }

    fn text(&self, n: &Node) -> &'a str {
        node_text(n, self.src)
    }

    /// The IDs of the object an expression gives.
    pub fn expr(&self, n: &Node<'t>) -> Ids {
        match n.kind() {
            "nil" | "number" | "string" | "bool" | "translation" | "unary_expression" | "map" => Some(BTreeSet::new()),
            "id" => {
                let t = self.text(n);
                if t.chars().all(|c| c.is_ascii_digit()) { Some(BTreeSet::new()) } else { one(t) }
            }
            "identifier" => {
                let name = self.text(n);
                if name == "this" {
                    return self.this_ids();
                }
                if self.params.contains(name) {
                    return None;
                }
                if self.locals.contains(name) {
                    return self.var(name);
                }
                if super::engine::ENGINE.constants.contains_key(name) {
                    return Some(BTreeSet::new());
                }
                self.field(name)
            }
            "method_call" => self.call(n),
            "binary_expression" => {
                let nc: Vec<Node> = named_children(n).into_iter().filter(|c| c.kind() != "comment").collect();
                let op = super::index::children(n).iter().find(|c| !c.is_named() && !matches!(c.kind(), "(" | ")")).map(|c| c.kind().to_string()).unwrap_or_default();
                match (op.as_str(), nc.first(), nc.last()) {
                    ("??" | "||" | "&&" | "..", Some(l), Some(r)) => union(self.expr(l), self.expr(r)),
                    ("=", _, Some(r)) => self.expr(r),
                    _ => Some(BTreeSet::new()), // arithmetic, comparisons
                }
            }
            "array" => named_children(n).iter().filter(|c| c.kind() != "comment").fold(Some(BTreeSet::new()), |acc, c| union(acc, self.expr(c))),
            "map_access" if !super::index::children(n).iter().any(|c| c.kind() == ".") => {
                named_children(n).first().and_then(|b| self.expr(b))
            }
            _ => None,
        }
    }

    /// The objects `this` can be: the script's definition and every definition that #includes it (a library's
    /// `this` is the including object), for an #appendto script its targets (and their includers); unknown for
    /// `#appendto *`, scenario/System scripts and inside a `global func` (the caller's context).
    fn this_ids(&self) -> Ids {
        if self.func.and_then(|f| f.child_by_field_name("visibility")).map(|v| self.text(&v) == "global").unwrap_or(false) {
            return None;
        }
        let roots: Vec<String> = if !self.info.appendto.is_empty() {
            if self.info.appendto.iter().any(|t| t == "*") {
                return None;
            }
            self.info.appendto.clone()
        } else {
            vec![self.info.def_id.clone()?]
        };
        let mut out = BTreeSet::new();
        let mut todo = roots;
        while let Some(id) = todo.pop() {
            if !out.insert(id.clone()) {
                continue;
            }
            for f in self.index.includers_of(&id) {
                if self.index.visible_from(&self.info.path, &f.path) || self.index.visible_from(&f.path, &self.info.path) {
                    if let Some(d) = &f.def_id {
                        todo.push(d.clone());
                    }
                }
            }
        }
        Some(out)
    }

    fn call(&self, n: &Node<'t>) -> Ids {
        let name = n.child_by_field_name("name").map(|x| self.text(&x)).unwrap_or("");
        let args: Vec<Node> = n.child_by_field_name("args").map(|a| named_children(&a)).unwrap_or_default()
            .into_iter().filter(|c| c.kind() != "comment").collect();
        if super::index::is_arrow_call(n) {
            return None;
        }
        if CREATORS.contains(&name) {
            if let Some(first) = args.first().filter(|f| f.kind() == "id") {
                return self.expr(first).filter(|s| !s.is_empty());
            }
        }
        if SEARCHES.contains(&name) {
            return self.criteria(&args);
        }
        // other engine functions: not an object unless they return one
        if let Some(t) = super::types::engine_return(name) {
            if t & (super::types::OBJECT | super::types::ARRAY) == 0 {
                return Some(BTreeSet::new());
            }
        }
        None
    }

    /// All criteria hold (several arguments, Find_And): the intersection of the restricting ones.
    fn criteria(&self, crits: &[Node<'t>]) -> Ids {
        let mut acc: Ids = None;
        for c in crits {
            if let Some(r) = self.criterion(c) {
                acc = Some(match acc {
                    Some(a) => a.intersection(&r).cloned().collect(),
                    None => r,
                });
            }
        }
        acc
    }

    fn criterion(&self, c: &Node<'t>) -> Ids {
        if c.kind() != "method_call" {
            return None;
        }
        let name = c.child_by_field_name("name").map(|x| self.text(&x)).unwrap_or("");
        let args: Vec<Node> = c.child_by_field_name("args").map(|a| named_children(&a)).unwrap_or_default()
            .into_iter().filter(|x| x.kind() != "comment").collect();
        match name {
            "Find_ID" => args.first().filter(|a| a.kind() == "id").and_then(|a| self.expr(a)).filter(|s| !s.is_empty()),
            "Find_And" => self.criteria(&args),
            "Find_Or" => args.iter().fold(Some(BTreeSet::new()), |acc, a| union(acc, self.criterion(a))).filter(|s| !s.is_empty()),
            _ => None,
        }
    }

    /// A var of the current function: the union over its assignments (cached; a cycle contributes nothing).
    fn var(&self, name: &str) -> Ids {
        if let Some(r) = self.cache.borrow().get(name) {
            return r.clone();
        }
        if !self.in_progress.borrow_mut().insert(name.to_string()) {
            return Some(BTreeSet::new());
        }
        let r = match self.func {
            Some(f) => assigned_values(&f, name, self.src, false).iter().fold(Some(BTreeSet::new()), |acc, v| union(acc, self.expr(v))),
            None => None,
        };
        self.in_progress.borrow_mut().remove(name);
        self.cache.borrow_mut().insert(name.to_string(), r.clone());
        r
    }

    /// A local/static: the union over the assignments in the scripts that write it (as in types::field_type).
    fn field(&self, name: &str) -> Ids {
        let vars = self.index.visible_vars(self.info, name);
        let (decl, var) = vars.first()?;
        if var.kind == VarKind::StaticConst {
            // static const X = FLNT: a definition (for X->F()); any other constant is no object
            let is_id = |v: &str| v.len() == 4 && v.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') && !v.chars().all(|c| c.is_ascii_digit());
            return match var.value.as_deref().map(str::trim) {
                Some(v) if is_id(v) => one(v),
                _ => Some(BTreeSet::new()),
            };
        }
        if self.depth >= 3 {
            return None;
        }
        let is_local = var.kind == VarKind::Local;
        let key = if is_local { format!("local|{}|{}", path_key(&decl.path), name) } else { format!("static|{}", name) };
        if let Some(r) = FIELD_IDS.lock().ok().and_then(|m| m.get(&key).cloned()) {
            return r;
        }
        if !FIELD_IN_PROGRESS.lock().map(|mut p| p.insert(key.clone())).unwrap_or(false) {
            return Some(BTreeSet::new());
        }
        let decl_fresh: &FileInfo = if path_key(&decl.path) == path_key(&self.info.path) { self.info } else { decl };
        let family: HashSet<String> = if is_local {
            self.index.family(decl_fresh).iter().map(|f| path_key(&f.path)).chain(std::iter::once(path_key(&decl.path))).collect()
        } else {
            HashSet::new()
        };
        let current = path_key(&self.info.path);
        let mut sites: Vec<(&FileInfo, usize, bool)> = self.index.writers(name).into_iter().filter(|(f, _, _)| path_key(&f.path) != current).collect();
        sites.extend(self.info.writes.iter().filter(|(n, _, _)| n == name).map(|(_, i, m)| (self.info, *i, *m)));
        let mut r: Ids = Some(BTreeSet::new());
        for (f, i, member) in sites {
            if member {
                r = None; // obj.name = x from somewhere: not followed
                break;
            }
            let fk = path_key(&f.path);
            if is_local && !family.contains(&fk) {
                continue;
            }
            if !is_local && self.index.visible_vars(f, name).first().map(|(_, v)| v.kind != VarKind::Static).unwrap_or(true) {
                continue;
            }
            r = union(r, self.ids_in(f, i, name));
            if r.is_none() {
                break;
            }
        }
        if let Ok(mut p) = FIELD_IN_PROGRESS.lock() {
            p.remove(&key);
        }
        if let Ok(mut m) = FIELD_IDS.lock() {
            m.insert(key, r.clone());
        }
        r
    }

    /// The IDs assigned to the script variable `name` in function `i` of `file`.
    fn ids_in(&self, file: &FileInfo, i: usize, name: &str) -> Ids {
        let f: &FuncDef = file.funcs.get(i)?;
        let current_key = path_key(&self.info.path);
        let entry = super::types::parsed_script(&file.path, Some((&current_key, self.src)))?;
        let (src, tree) = (&entry.0, &entry.1);
        let root = tree.root_node();
        let func = super::types::find_func(&root, src, f)?;
        let ctx = IdCtx::at_depth(self.index, file, src, Some(func), self.depth + 1);
        assigned_values(&func, name, src, true).iter().fold(Some(BTreeSet::new()), |acc, v| union(acc, ctx.expr(v)))
    }
}

/// The expressions assigned to `name` in a function: `name = v`, `var name = v`, `name[] = v` / `name[i] = v` (an
/// array's elements), `name ..= v`, `name ??= v`, and the collection of `for (var name in coll)`. `script_var`: only
/// assignments where `name` isn't a parameter/var of the function.
fn assigned_values<'t>(func: &Node<'t>, name: &str, src: &str, script_var: bool) -> Vec<Node<'t>> {
    let mut out = vec![];
    let mut stack = vec![*func];
    let mut shadowed = false;
    while let Some(n) = stack.pop() {
        match n.kind() {
            "param" if n.child_by_field_name("name").map(|x| node_text(&x, src) == name).unwrap_or(false) => shadowed = true,
            "var_assignment" => {
                let nc = named_children(&n);
                if nc.first().map(|x| node_text(x, src) == name).unwrap_or(false) {
                    shadowed = true;
                    out.extend(nc.get(1).copied());
                }
            }
            "for_statement" => {
                if for_in_vars(&n).iter().any(|v| node_text(v, src) == name) {
                    let kids = super::index::children(&n);
                    if let Some(at) = kids.iter().position(|c| c.kind() == "in") {
                        out.extend(kids[at + 1..].iter().find(|c| c.is_named()).copied());
                    }
                }
            }
            "binary_expression" => {
                let nc: Vec<Node> = named_children(&n).into_iter().filter(|c| c.kind() != "comment").collect();
                let op = super::index::children(&n).iter().find(|c| !c.is_named() && !matches!(c.kind(), "(" | ")")).map(|c| c.kind().to_string()).unwrap_or_default();
                if let (true, Some(l), Some(r)) = (matches!(op.as_str(), "=" | "..=" | "??="), nc.first(), nc.last()) {
                    let target = match l.kind() {
                        "identifier" => Some(*l),
                        "array_append" => named_children(l).into_iter().next(),
                        "map_access" if !super::index::children(l).iter().any(|c| c.kind() == ".") => named_children(l).into_iter().next(),
                        _ => None,
                    };
                    if target.map(|t| t.kind() == "identifier" && node_text(&t, src) == name).unwrap_or(false) {
                        out.push(*r);
                    }
                }
            }
            _ => {}
        }
        stack.extend(named_children(&n));
    }
    if script_var && shadowed {
        return vec![];
    }
    out
}

/// IDs of the receiver of the `->` call `call` (a method_call node), if they are known and every one of them has an
/// indexed script; None otherwise (or switched off).
pub fn receiver_ids(index: &Index, info: &FileInfo, src: &str, call: &Node) -> Option<Vec<String>> {
    if !super::diagnostics::object_ids_enabled() {
        return None;
    }
    let arrow = call.parent().filter(|p| p.kind() == "arrow_expression")?;
    let target = named_children(&arrow).into_iter().find(|c| c.kind() != "comment")?;
    if target.id() == call.id() {
        return None;
    }
    let ctx = IdCtx::new(index, info, src, enclosing_function(call));
    resolvable(index, info, ctx.expr(&target)?)
}

/// IDs of a variable / expression node (hover); None when unknown or not an object.
pub fn ids_of(index: &Index, info: &FileInfo, src: &str, node: &Node) -> Option<Vec<String>> {
    if !super::diagnostics::object_ids_enabled() {
        return None;
    }
    let ctx = IdCtx::new(index, info, src, enclosing_function(node));
    let ids = ctx.expr(node)?;
    (!ids.is_empty()).then(|| ids.into_iter().collect())
}

/// Known IDs whose scripts are all indexed (a definition from an unindexed pack: its functions are unknown).
fn resolvable(index: &Index, info: &FileInfo, ids: BTreeSet<String>) -> Option<Vec<String>> {
    if ids.is_empty() {
        return None;
    }
    for id in &ids {
        if info.def_id.as_deref() == Some(id.as_str()) {
            continue;
        }
        let i = index.id_for(id, &info.path)?;
        if let Some(s) = &i.script {
            // the script, and everything it #includes / #appendto, must be indexed
            match index.files.get(&path_key(s)) {
                Some(f) if !index.has_unresolved_targets(f) => {}
                _ => return None,
            }
        }
    }
    Some(ids.into_iter().collect())
}

/// The scripts making up the definition `id` as seen from the calling script `info`, with a rank (lower wins on a
/// name clash): its #appendto scripts (0), its own script (1), then its #include chain (each included definition's
/// appenders before it, 2, 3, ...). Appenders only count where the caller's game loads them; an appender's other
/// targets are not part of this definition.
pub fn def_scripts<'a>(index: &'a Index, info: &'a FileInfo, id: &str) -> Vec<(usize, &'a FileInfo)> {
    let mut out: Vec<(usize, &FileInfo)> = vec![];
    let mut seen = HashSet::new();
    let mut level = 0;
    let mut cur: Option<(String, &FileInfo)> = if info.def_id.as_deref() == Some(id) {
        Some((id.to_string(), info))
    } else {
        index.script_of(id, &info.path).map(|f| (id.to_string(), f))
    };
    let mut todo: Vec<(String, &FileInfo)> = vec![];
    while let Some((did, script)) = cur.take().or_else(|| todo.pop()) {
        if !seen.insert(path_key(&script.path)) {
            continue;
        }
        for a in index.appenders_of(&did) {
            let a = if path_key(&a.path) == path_key(&info.path) { info } else { a };
            if index.visible_from(&info.path, &a.path) && seen.insert(path_key(&a.path)) {
                out.push((level, a));
            }
        }
        out.push((level + 1, script));
        level += 2;
        for inc in script.includes.iter().rev() {
            if let Some(s) = index.script_of(inc, &info.path) {
                todo.push((inc.clone(), s));
            }
        }
    }
    out
}

/// The script functions named `name` an `->` call on an object of one of `ids` reaches: the definitions' scripts
/// with their #include/#appendto scripts, plus global functions (C4AulFunc::FindSameNameFunc).
pub fn member_funcs<'a>(index: &'a Index, info: &'a FileInfo, ids: &[String], name: &str) -> Vec<(&'a FileInfo, &'a FuncDef)> {
    let mut out: Vec<(&FileInfo, &FuncDef)> = vec![];
    for id in ids {
        let scripts = def_scripts(index, info, id);
        let defining: Vec<(usize, &FileInfo, &FuncDef)> = scripts.iter()
            .flat_map(|(rank, f)| f.funcs.iter().filter(|d| d.name == name).map(move |d| (*rank, *f, d)))
            .collect();
        // the function that runs is the one of the best rank (C4AulFunc OverloadedBy)
        if let Some(best) = defining.iter().map(|(r, _, _)| *r).min() {
            let defining: Vec<(&FileInfo, &FuncDef)> = defining.into_iter().filter(|(r, _, _)| *r == best).map(|(_, f, d)| (f, d)).collect();
            for (f, d) in defining {
                if !out.iter().any(|(g, e)| path_key(&g.path) == path_key(&f.path) && e.name_range == d.name_range) {
                    out.push((f, d));
                }
            }
        }
    }
    for g in index.global_funcs(&info.path, name) {
        if !out.iter().any(|(f, d)| path_key(&f.path) == path_key(&g.0.path) && d.name_range == g.1.name_range) {
            out.push(g);
        }
    }
    out
}
