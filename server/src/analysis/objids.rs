//! Which definitions (IDs) an object value can be of.
//!
//! Flow-insensitive and syntactic: a variable's IDs are the union over every value assigned to it in its function
//! (a `local`: in its definition family, like types.rs field_type), read off the assigned expression:
//! `CreateObject(ID, ...)` / `CreateContents` / `CreateConstruction` / `PlaceAnimal` / `PlaceVegetation` /
//! `FindObject(ID, ...)` / `FindContents(ID)` -> {ID}; `FindObject2`/`FindObjects`/`FindObject` criteria with
//! `Find_ID(ID)` (Find_Or -> union, Find_And / several criteria -> intersection); `this` -> the script's definition;
//! an ID value (definition calls); another variable; `a ?? b`, `a || b`, `[a, b]`, `a[i]` (an array's elements);
//! nil/numbers/strings add nothing; a script function call (also `obj->F()` on a known object): the union over its
//! `return` values. Anything else (parameters, obj.local, `Contents()`) is unknown.
//! Narrowing: a use inside `if (GetID(x) == ID)` / `x->GetID() == ID` (also `||`, `&&`, `!`, the else branch of `!=`,
//! `while`, the right side of `&&`/`||` from #strict 2 on) or after `if (GetID(x) != ID) return;` is of ID, unless
//! x is assigned in between (a loop around the use counts as a whole).
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
    /// "<script>|func|line" -> IDs of the function's return values
    static ref RETURN_IDS: Mutex<HashMap<String, Ids>> = Mutex::new(HashMap::new());
    static ref RETURN_IN_PROGRESS: Mutex<HashSet<String>> = Mutex::new(HashSet::new());
}

pub fn clear_cache() {
    if let Ok(mut m) = FIELD_IDS.lock() {
        m.clear();
    }
    if let Ok(mut m) = RETURN_IDS.lock() {
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
                if let Some(r) = self.narrow(n, name) {
                    return Some(r);
                }
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
            return self.arrow_call(n, name);
        }
        if CREATORS.contains(&name) {
            if let Some(first) = args.first().filter(|f| f.kind() == "id") {
                return self.expr(first).filter(|s| !s.is_empty());
            }
        }
        if SEARCHES.contains(&name) {
            return self.criteria(&args);
        }
        if matches!(name, "Par" | "inherited" | "_inherited") {
            return None;
        }
        if let Some(r) = self.script_call(name) {
            return r;
        }
        // other engine functions: not an object unless they return one
        if let Some(t) = super::types::engine_return(name) {
            if t & (super::types::OBJECT | super::types::ARRAY) == 0 {
                return Some(BTreeSet::new());
            }
        }
        None
    }

    fn in_global(&self) -> bool {
        self.func.and_then(|f| f.child_by_field_name("visibility")).map(|v| self.text(&v) == "global").unwrap_or(false)
    }

    /// A direct call of a script function: the union over the return values of every function it can reach (the
    /// object may be one that #includes this script and overloads the name). None: no script function of that name.
    fn script_call(&self, name: &str) -> Option<Ids> {
        let mut funcs = if self.in_global() { self.index.global_funcs(&self.info.path, name) } else { self.index.visible_funcs(self.info, name) };
        if funcs.is_empty() {
            return None;
        }
        if !self.in_global() {
            if let Some(ids) = self.this_ids() {
                let ids: Vec<String> = ids.into_iter().collect();
                for g in member_funcs(self.index, self.info, &ids, name) {
                    if !funcs.iter().any(|(f, d)| path_key(&f.path) == path_key(&g.0.path) && d.name_range == g.1.name_range) {
                        funcs.push(g);
                    }
                }
            }
        }
        if funcs.len() > 8 {
            return Some(None);
        }
        Some(funcs.iter().fold(Some(BTreeSet::new()), |acc, (f, d)| union(acc, self.returns(f, d))))
    }

    /// `obj->F()` on an object whose definitions are known and indexed: the return values of the function that runs.
    fn arrow_call(&self, n: &Node<'t>, name: &str) -> Ids {
        let target = named_children(&n.parent()?).into_iter().find(|c| c.kind() != "comment")?;
        let ids = resolvable(self.index, self.info, self.expr(&target)?)?;
        let funcs = member_funcs(self.index, self.info, &ids, name);
        if funcs.is_empty() {
            let t = super::types::engine_return(name)?;
            return (t & (super::types::OBJECT | super::types::ARRAY) == 0).then(BTreeSet::new);
        }
        funcs.iter().fold(Some(BTreeSet::new()), |acc, (f, d)| union(acc, self.returns(f, d)))
    }

    /// IDs of a script function's return values (cached; recursion and deep chains give unknown).
    fn returns(&self, file: &FileInfo, f: &FuncDef) -> Ids {
        if self.depth >= 4 {
            return None;
        }
        let key = format!("{}|{}|{}", path_key(&file.path), f.name, f.name_range.start.line);
        if let Some(r) = RETURN_IDS.lock().ok().and_then(|m| m.get(&key).cloned()) {
            return r;
        }
        if !RETURN_IN_PROGRESS.lock().map(|mut p| p.insert(super::types::busy_key(&key))).unwrap_or(false) {
            return None;
        }
        let r = (|| {
            let current_key = path_key(&self.info.path);
            let entry = super::types::parsed_script(&file.path, Some((&current_key, self.src)))?;
            let (src, tree) = (&entry.0, &entry.1);
            let root = tree.root_node();
            let func = super::types::find_func(&root, src, f)?;
            let ctx = IdCtx::at_depth(self.index, file, src, Some(func), self.depth + 1);
            let mut acc = Some(BTreeSet::new());
            let mut stack = vec![func];
            while let Some(n) = stack.pop() {
                if n.kind() == "return_statement" {
                    if let Some(v) = named_children(&n).into_iter().find(|c| c.kind() != "comment") {
                        acc = union(acc, ctx.expr(&v));
                    }
                    if acc.is_none() {
                        break;
                    }
                }
                stack.extend(named_children(&n));
            }
            acc
        })();
        if let Ok(mut p) = RETURN_IN_PROGRESS.lock() {
            p.remove(&super::types::busy_key(&key));
        }
        if let Ok(mut m) = RETURN_IDS.lock() {
            m.insert(key, r.clone());
        }
        r
    }

    /// The definition `name` (a variable or `this`) must be of at the use `n`, from GetID tests guarding it.
    fn narrow(&self, n: &Node<'t>, name: &str) -> Option<BTreeSet<String>> {
        let func = self.func?;
        let mut c = *n;
        while let Some(p) = c.parent() {
            if p.id() == func.id() {
                break;
            }
            let named: Vec<Node> = named_children(&p).into_iter().filter(|x| x.kind() != "comment").collect();
            match p.kind() {
                "if_statement" if named.first().map(|f| f.id() != c.id()).unwrap_or(false) => {
                    let cond = named[0];
                    let alt = super::index::children(&p).iter().find(|x| x.kind() == "else").map(|e| c.start_byte() > e.start_byte()).unwrap_or(false);
                    if let Some(r) = self.cond_ids(&cond, name, !alt).filter(|_| !self.reassigned(name, cond.end_byte(), n)) {
                        return Some(r);
                    }
                }
                "while_statement" if named.first().map(|f| f.id() != c.id()).unwrap_or(false) => {
                    if let Some(r) = self.cond_ids(&named[0], name, true).filter(|_| !self.reassigned(name, named[0].end_byte(), &p)) {
                        return Some(r);
                    }
                }
                "binary_expression" if named.len() == 2 && named[1].id() == c.id() => {
                    let op = super::index::children(&p).iter().find(|x| !x.is_named() && !matches!(x.kind(), "(" | ")")).map(|x| x.kind().to_string()).unwrap_or_default();
                    if matches!(op.as_str(), "&&" | "||") && self.strict() >= 2 {
                        if let Some(r) = self.cond_ids(&named[0], name, op == "&&").filter(|_| !self.reassigned(name, named[0].end_byte(), n)) {
                            return Some(r);
                        }
                    }
                }
                "block" => {
                    // if (GetID(x) != ID) return; ... x
                    for s in named.iter().rev().filter(|s| s.end_byte() <= c.start_byte()) {
                        if s.kind() != "if_statement" || super::index::children(s).iter().any(|x| x.kind() == "else") {
                            continue;
                        }
                        let parts: Vec<Node> = named_children(s).into_iter().filter(|x| x.kind() != "comment").collect();
                        if let (Some(cond), Some(body)) = (parts.first(), parts.get(1)) {
                            if exits(body) {
                                if let Some(r) = self.cond_ids(cond, name, false).filter(|_| !self.reassigned(name, s.end_byte(), n)) {
                                    return Some(r);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            c = p;
        }
        None
    }

    fn strict(&self) -> u8 {
        let mut root = match self.func { Some(f) => f, None => return 0 };
        while let Some(p) = root.parent() {
            root = p;
        }
        super::diagnostics::strict_level(&root, self.src)
    }

    /// What `cond` being `truth` says about the definition of `name`.
    fn cond_ids(&self, cond: &Node<'t>, name: &str, truth: bool) -> Option<BTreeSet<String>> {
        let nc: Vec<Node> = named_children(cond).into_iter().filter(|c| c.kind() != "comment").collect();
        let op = super::index::children(cond).iter().find(|c| !c.is_named() && !matches!(c.kind(), "(" | ")")).map(|c| c.kind().to_string()).unwrap_or_default();
        match (cond.kind(), op.as_str()) {
            ("unary_expression", "!") => self.cond_ids(nc.first()?, name, !truth),
            ("binary_expression", "==" | "!=") if (op == "==") == truth => {
                let (l, r) = (nc.first()?, nc.get(1)?);
                let id = |x: &Node| (x.kind() == "id" && !self.text(x).chars().all(|ch| ch.is_ascii_digit())).then(|| self.text(x).to_string());
                let (g, i) = if self.is_get_id(l, name) { (l, id(r)?) } else { (r, id(l)?) };
                self.is_get_id(g, name).then(|| std::iter::once(i).collect())
            }
            // both hold: either one narrows (both: the intersection)
            ("binary_expression", "&&" | "||") if (op == "&&") == truth => {
                let (a, b) = (self.cond_ids(nc.first()?, name, truth), self.cond_ids(nc.get(1)?, name, truth));
                match (a, b) {
                    (Some(a), Some(b)) => Some(a.intersection(&b).cloned().collect()),
                    (a, b) => a.or(b),
                }
            }
            // one of them holds: both must narrow
            ("binary_expression", "&&" | "||") => {
                let mut a = self.cond_ids(nc.first()?, name, truth)?;
                a.extend(self.cond_ids(nc.get(1)?, name, truth)?);
                Some(a)
            }
            _ => None,
        }
    }

    /// `GetID(name)`, `name->GetID()`, for `this` also `GetID()`.
    fn is_get_id(&self, n: &Node<'t>, name: &str) -> bool {
        let is_name = |x: &Node| x.kind() == "identifier" && self.text(x) == name;
        let get_id = |x: &Node| x.kind() == "method_call" && x.child_by_field_name("name").map(|y| self.text(&y) == "GetID").unwrap_or(false);
        let args = |x: &Node<'t>| -> Vec<Node<'t>> {
            x.child_by_field_name("args").map(|a| named_children(&a)).unwrap_or_default().into_iter().filter(|c| c.kind() != "comment").collect()
        };
        match n.kind() {
            "method_call" if get_id(n) && !super::index::is_arrow_call(n) => match args(n).as_slice() {
                [] => name == "this",
                [a] => is_name(a),
                _ => false,
            },
            "arrow_expression" => {
                let nc: Vec<Node> = named_children(n).into_iter().filter(|c| c.kind() != "comment").collect();
                nc.len() == 2 && is_name(&nc[0]) && get_id(&nc[1]) && args(&nc[1]).is_empty()
            }
            _ => false,
        }
    }

    /// Whether `name` is assigned between byte `from` and the use `n` (a loop around `n` that starts after `from`
    /// counts as a whole: its later assignments reach `n` through the next iteration).
    fn reassigned(&self, name: &str, from: usize, n: &Node) -> bool {
        let Some(func) = self.func else { return true };
        let mut to = n.start_byte();
        let mut cur = n.parent();
        while let Some(p) = cur {
            if matches!(p.kind(), "while_statement" | "for_statement") && p.start_byte() >= from {
                to = to.max(p.end_byte());
            }
            cur = p.parent();
        }
        assigned_values(&func, name, self.src, false).iter().any(|v| v.start_byte() >= from && v.start_byte() < to)
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
        if !FIELD_IN_PROGRESS.lock().map(|mut p| p.insert(super::types::busy_key(&key))).unwrap_or(false) {
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
        // passed to a reference parameter: what the callee writes isn't followed
        if !self.index.ref_writes(self.info, name).is_empty() {
            sites.push((self.info, usize::MAX, true));
        }
        let mut r: Ids = Some(BTreeSet::new());
        for (f, i, member) in sites {
            if member || self.index.ref_writes(f, name).contains(&i) {
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
            p.remove(&super::types::busy_key(&key));
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
pub(crate) fn assigned_values<'t>(func: &Node<'t>, name: &str, src: &str, script_var: bool) -> Vec<Node<'t>> {
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

/// Whether a statement always leaves the code after it (return/break/continue, a block ending in one, an
/// if/else whose branches both leave).
fn exits(s: &Node) -> bool {
    let named: Vec<Node> = named_children(s).into_iter().filter(|c| !matches!(c.kind(), "comment" | "else")).collect();
    match s.kind() {
        "return_statement" | "flow_control_statement" => true,
        "block" => named.last().map(exits).unwrap_or(false),
        "if_statement" => super::index::children(s).iter().any(|c| c.kind() == "else") && named.len() == 3 && exits(&named[1]) && exits(&named[2]),
        _ => false,
    }
}

/// IDs of the receiver of the `->` call `call` (a method_call node), if they are known and every one of them has an
/// indexed script; None otherwise (or switched off).
pub fn receiver_ids(index: &Index, info: &FileInfo, src: &str, call: &Node) -> Option<Vec<String>> {
    if !super::diagnostics::object_ids_enabled() {
        return None;
    }
    let arrow = call.parent().filter(|p| p.kind() == "arrow_expression")?;
    // obj->ID::F(): exactly ID's function, whatever obj is
    if let Some(idn) = call.child_by_field_name("id") {
        let id = node_text(&idn, src).to_string();
        return resolvable(index, info, std::iter::once(id).collect());
    }
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
