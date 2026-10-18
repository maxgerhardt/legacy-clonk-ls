//! Workspace index: functions, variables, calls and definition IDs of all scripts below the roots.

use super::text::{decode_bytes, node_text, LineIndex};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tower_lsp::lsp_types::{Range, Url};
use tree_sitter::{Node, Parser, Tree};

#[derive(Debug, Clone)]
pub struct FuncDef {
    pub name: String,
    /// (type, name); type "&" = a reference parameter (`&x`, `int &x`: the engine drops the type, C4AulParse)
    pub params: Vec<(Option<String>, String)>,
    pub visibility: Option<String>,
    pub name_range: Range,
    pub range: Range,
    pub doc: Option<String>,
}

impl FuncDef {
    pub fn signature(&self) -> String {
        let params = self
            .params
            .iter()
            .map(|(t, n)| match t {
                Some(t) if t == "&" => format!("&{}", n),
                Some(t) => format!("{} {}", t, n),
                None => n.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        match &self.visibility {
            Some(v) => format!("{} func {}({})", v, self.name, params),
            None => format!("func {}({})", self.name, params),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VarKind {
    Local,
    Static,
    StaticConst,
}

#[derive(Debug, Clone)]
pub struct VarDef {
    pub name: String,
    pub kind: VarKind,
    pub range: Range,
    pub value: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CallSite {
    pub name: String,
    pub range: Range,
    /// start byte of the name in the script source
    pub byte: usize,
    pub arrow: bool,
    pub args: usize,
}

#[derive(Debug, Clone)]
pub struct FileInfo {
    pub path: PathBuf,
    /// ID of the definition this script belongs to (from DefCore.txt next to it)
    pub def_id: Option<String>,
    /// Functions of System.c4g scripts are all global
    pub is_system: bool,
    pub includes: Vec<String>,
    pub appendto: Vec<String>,
    pub funcs: Vec<FuncDef>,
    pub vars: Vec<VarDef>,
    pub calls: Vec<CallSite>,
    pub id_refs: Vec<(String, Range)>,
    /// script variables assigned in a function: (name, function index, member) - member = obj.name = x or
    /// LocalN("name") = x, else a plain assignment of a name that isn't a parameter/var of the function
    pub writes: Vec<(String, usize, bool)>,
    /// script variables passed as call arguments: (name, function index, callee, argument position); a write site
    /// when the callee has a reference parameter there (Lookup::build -> writers, Index::is_ref_param)
    pub ref_args: Vec<(String, usize, String, usize)>,
    /// identifiers inside the script's strings and the *Call= values of the ActMap.txt/DefCore.txt next to it
    /// (functions that may be called by name)
    pub string_words: HashSet<String>,
    /// identifiers in strings that call on the script's own object: Schedule("F()") (no ->), ScheduleCall(0/this,
    /// "F"), AddTimer("F"), Call("F"), PrivateCall/ProtectedCall(this, "F") - they reach only this definition family
    pub own_string_words: HashSet<String>,
    /// the text before a format placeholder in a string (Call(Format("Fire%d", i)) may call Fire1, Fire2, ...)
    pub string_prefixes: HashSet<String>,
    /// functions the ActMap.txt / DefCore.txt next to the script calls: (function, key like "PhaseCall", the
    /// action's Procedure or "" for DefCore)
    pub ini_calls: Vec<(String, String, String)>,
    /// names read somewhere in the script (identifiers that aren't declarations, plain `x =` targets, function or
    /// parameter names; LocalN("x") strings), and whether it uses Local(n) (locals by index): lint unused-local
    pub read_idents: HashSet<String>,
    pub locals_by_index: bool,
}

/// FileInfo::read_idents / locals_by_index.
fn collect_reads(root: &Node, src: &str, info: &mut FileInfo) {
    let mut stack = vec![*root];
    while let Some(n) = stack.pop() {
        match n.kind() {
            // declarations: the name isn't a read, the initial value is
            "var_assignment" => {
                stack.extend(named_children(&n).into_iter().skip(1));
                continue;
            }
            "param" | "function_definition" | "old_function_definition" => {
                if n.kind() != "param" {
                    stack.extend(named_children(&n).into_iter().filter(|c| c.kind() != "identifier" || n.child_by_field_name("name").map(|x| x.id() != c.id()).unwrap_or(true)));
                }
                continue;
            }
            "binary_expression" if children(&n).iter().any(|c| c.kind() == "=") => {
                let nc: Vec<Node> = named_children(&n).into_iter().filter(|c| c.kind() != "comment").collect();
                if nc.first().map(|l| l.kind() == "identifier").unwrap_or(false) {
                    stack.extend(nc.into_iter().skip(1));
                    continue;
                }
            }
            "method_call" => {
                let name = n.child_by_field_name("name").map(|x| node_text(&x, src)).unwrap_or("");
                // Local(n) reads locals by index, eval("...") may name any of them
                if name == "Local" || name == "eval" {
                    info.locals_by_index = true;
                }
                if name == "LocalN" {
                    if let Some(s) = n.child_by_field_name("args").and_then(|a| named_children(&a).into_iter().find(|c| c.kind() == "string")) {
                        info.read_idents.insert(node_text(&s, src).trim_matches('"').to_string());
                    }
                }
                // the called name isn't a variable read; the receiver and arguments are
                stack.extend(named_children(&n).into_iter().filter(|c| n.child_by_field_name("name").map(|x| x.id() != c.id()).unwrap_or(true)));
                continue;
            }
            "identifier" => {
                info.read_idents.insert(node_text(&n, src).to_string());
            }
            _ => {}
        }
        stack.extend(named_children(&n));
    }
}

/// Whether a string is a call by name on the script's own object (see FileInfo::own_string_words).
fn own_object_string(s: &Node, src: &str) -> bool {
    let Some(args) = s.parent().filter(|p| p.kind() == "args_list") else { return false };
    let Some(call) = args.parent().filter(|c| c.kind() == "method_call" && !is_arrow_call(c)) else { return false };
    let name = call.child_by_field_name("name").map(|x| node_text(&x, src)).unwrap_or("");
    let list: Vec<Node> = named_children(&args).into_iter().filter(|c| c.kind() != "comment").collect();
    let pos = list.iter().position(|c| c.id() == s.id());
    let first_is_self = list.first().map(|f| matches!(node_text(f, src), "0" | "this" | "nil" | "this()")).unwrap_or(false);
    match (name, pos) {
        ("Schedule", Some(0)) => !node_text(s, src).contains("->"),
        ("AddTimer", Some(0)) | ("Call", Some(0)) => true,
        ("ScheduleCall" | "PrivateCall" | "ProtectedCall", Some(1)) => first_is_self,
        _ => false,
    }
}

/// Engine functions that run a string as script or call a function by its name.
const BY_NAME: &[&str] = &["Schedule", "ScheduleCall", "AddTimer", "Call", "PrivateCall", "ProtectedCall", "ObjectCall",
    "GameCall", "GameCallEx", "DefinitionCall", "eval", "AddMenuItem", "CreateMenu", "SetCommand", "AddCommand",
    "AppendCommand", "AddEffect", "EffectCall", "CheckEffect", "GetEffect", "RemoveEffect", "ChangeEffect"];

/// Whether a string can name a function that gets called: false when it goes straight (maybe through Format) into
/// an engine function that doesn't call by name (SetAction, Sound, Log, Message, ...) or into a comparison;
/// anything else (by-name calls, script functions, variables, return values) may call.
fn string_may_call(s: &Node, src: &str) -> bool {
    let mut cur = *s;
    loop {
        let Some(p) = cur.parent() else { return true };
        match p.kind() {
            "args_list" => {
                let Some(call) = p.parent().filter(|c| c.kind() == "method_call") else { return true };
                let name = call.child_by_field_name("name").map(|x| node_text(&x, src)).unwrap_or("");
                if name == "Format" {
                    cur = call;
                    continue;
                }
                return BY_NAME.contains(&name) || !super::engine::ENGINE.functions.contains_key(name) || is_arrow_call(&call);
            }
            "binary_expression" => {
                let op = children(&p).iter().find(|c| !c.is_named() && !matches!(c.kind(), "(" | ")")).map(|c| c.kind().to_string()).unwrap_or_default();
                return !matches!(op.as_str(), "==" | "!=" | "eq" | "ne" | "S=");
            }
            _ => return true,
        }
    }
}

/// Collect the identifiers of a string (and the identifier right before a '%').
fn string_words(text: &str, words: &mut HashSet<String>, prefixes: &mut HashSet<String>) {
    let mut cur = String::new();
    for c in text.chars().chain(std::iter::once(' ')) {
        if c.is_ascii_alphanumeric() || c == '_' {
            cur.push(c);
            continue;
        }
        if !cur.is_empty() && !cur.chars().next().unwrap().is_ascii_digit() {
            if c == '%' {
                prefixes.insert(cur.clone());
            }
            words.insert(std::mem::take(&mut cur));
        }
        cur.clear();
    }
}

/// Functions an ActMap.txt / DefCore.txt next to a script calls by name: (function, key, the action's Procedure).
fn ini_calls(dir: &Path) -> Vec<(String, String, String)> {
    let mut out = vec![];
    for f in ["ActMap.txt", "DefCore.txt"] {
        let Ok(bytes) = std::fs::read(dir.join(f)) else { continue };
        let text = super::text::decode_bytes(&bytes);
        // per section: its Procedure and the *Call= entries
        let mut sections: Vec<(String, Vec<(String, String)>)> = vec![(String::new(), vec![])];
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                sections.push((String::new(), vec![]));
            } else if let Some((k, v)) = line.split_once('=') {
                let (k, v) = (k.trim(), v.trim());
                let sec = sections.last_mut().unwrap();
                if k == "Procedure" {
                    sec.0 = v.to_string();
                } else if k.ends_with("Call") && !v.is_empty() {
                    sec.1.push((v.to_string(), k.to_string()));
                }
            }
        }
        for (procedure, calls) in sections {
            for (func, key) in calls {
                out.push((func, key, procedure.clone()));
            }
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct IdInfo {
    pub id: String,
    pub name: Option<String>,
    pub defcore: PathBuf,
    pub line: u32,
    pub script: Option<PathBuf>,
}

#[derive(Default)]
pub struct Index {
    pub roots: Vec<PathBuf>,
    pub files: HashMap<String, FileInfo>,
    /// definition ID -> all definitions with that ID (scenarios may each have their own)
    pub ids: HashMap<String, Vec<IdInfo>>,
    /// Packs named in `DefinitionN=` lines of the Scenario.txt files found while scanning (e.g. "Knights.c4d")
    pub referenced_packs: HashSet<String>,
    /// scenario folder key (path_key + "/") -> file names (lowercase) of the packs it loads
    pub scenario_packs: HashMap<String, Vec<String>>,
    /// Extra folders indexed because a script inside them was opened (see `cover`)
    pub covered: Vec<PathBuf>,
    /// Name tables for fast lookups, built on first use and reset by every change of `files`
    lookup: std::sync::OnceLock<Lookup>,
    /// scope_files results ("<script>|<#include/#appendto lines>" -> file keys after the script itself); reset with
    /// `lookup`. The type inference asks for the same script's scope thousands of times.
    scope_cache: std::sync::Mutex<HashMap<String, Vec<String>>>,
}

/// Name -> (file key, index into FileInfo::funcs/vars) tables, so lookups don't scan all files.
#[derive(Default)]
struct Lookup {
    funcs: HashMap<String, Vec<(String, usize)>>,
    globals: HashMap<String, Vec<(String, usize)>>,
    statics: HashMap<String, Vec<(String, usize)>>,
    /// #appendto target ID (or "*") -> scripts appending to it
    appenders: HashMap<String, Vec<String>>,
    /// #include target ID -> scripts including it
    includers: HashMap<String, Vec<String>>,
    /// variable name -> (script, function index, member write)
    writers: HashMap<String, Vec<(String, usize, bool)>>,
    /// function name -> call sites (script, index in its calls)
    call_sites: HashMap<String, Vec<(String, usize)>>,
    /// every name in some script's strings / ActMap / DefCore calls, and format prefixes (FileInfo::string_words)
    string_words: HashSet<String>,
    string_prefixes: HashSet<String>,
    /// function -> its ActMap/DefCore uses (key, procedure)
    ini_calls: HashMap<String, Vec<(String, String, String)>>,
    /// name -> number of scripts reading it (FileInfo::read_idents, also their string words)
    read_counts: HashMap<String, u32>,
    /// (function, argument position) of reference parameters (script &x, engine C4Value *)
    ref_params: HashSet<(String, usize)>,
    /// lowercase names of the .c4d/.c4f folders that indexed scripts lie in (which packs are indexed)
    pack_dirs: HashSet<String>,
}

impl Lookup {
    fn build(files: &HashMap<String, FileInfo>) -> Lookup {
        let mut l = Lookup::default();
        // (function, position) of every reference parameter: script &x and engine C4Value * parameters
        let mut refs: HashSet<(String, usize)> = HashSet::new();
        for f in files.values() {
            for func in &f.funcs {
                for (i, p) in func.params.iter().enumerate() {
                    if p.0.as_deref() == Some("&") {
                        refs.insert((func.name.clone(), i));
                    }
                }
            }
        }
        for (name, ef) in &super::engine::ENGINE.functions {
            for (i, p) in ef.params.iter().flatten().enumerate() {
                if p.etype.starts_with('&') {
                    refs.insert((name.clone(), i));
                }
            }
        }
        for (key, f) in files {
            for (i, func) in f.funcs.iter().enumerate() {
                l.funcs.entry(func.name.clone()).or_default().push((key.clone(), i));
                // only 'global func' is global, also in System.c4g: a plain func there belongs to its script
                // (C4AulFuncMap::GetFunc looks up by owner; oracle case TS27: "unknown identifier" from a definition)
                if func.visibility.as_deref() == Some("global") {
                    l.globals.entry(func.name.clone()).or_default().push((key.clone(), i));
                }
            }
            for (i, v) in f.vars.iter().enumerate() {
                if v.kind != VarKind::Local {
                    l.statics.entry(v.name.clone()).or_default().push((key.clone(), i));
                }
            }
            for t in &f.appendto {
                l.appenders.entry(t.clone()).or_default().push(key.clone());
            }
            for t in &f.includes {
                l.includers.entry(t.clone()).or_default().push(key.clone());
            }
            for (name, i, member) in &f.writes {
                l.writers.entry(name.clone()).or_default().push((key.clone(), *i, *member));
            }
            for (i, c) in f.calls.iter().enumerate() {
                l.call_sites.entry(c.name.clone()).or_default().push((key.clone(), i));
            }
            l.string_words.extend(f.string_words.iter().cloned());
            l.string_prefixes.extend(f.string_prefixes.iter().cloned());
            for n in f.read_idents.iter().chain(f.string_words.iter()).chain(f.own_string_words.iter()).collect::<HashSet<_>>() {
                *l.read_counts.entry(n.clone()).or_insert(0) += 1;
            }
            for (func, k, p) in &f.ini_calls {
                l.ini_calls.entry(func.clone()).or_default().push((key.clone(), k.clone(), p.clone()));
            }
            for (name, i, callee, pos) in &f.ref_args {
                if refs.contains(&(callee.clone(), *pos)) && !f.writes.iter().any(|(n, j, m)| n == name && j == i && !m) {
                    l.writers.entry(name.clone()).or_default().push((key.clone(), *i, false));
                }
            }
            for seg in key.split('/') {
                let seg = seg.to_lowercase();
                if seg.ends_with(".c4d") || seg.ends_with(".c4f") {
                    l.pack_dirs.insert(seg);
                }
            }
        }
        l.ref_params = refs;
        l
    }
}

pub fn path_key(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/").to_lowercase()
}

pub fn uri_to_path(uri: &Url) -> Option<PathBuf> {
    uri.to_file_path().ok()
}

pub fn path_to_uri(p: &Path) -> Option<Url> {
    Url::from_file_path(p).ok()
}

/// Clonk scripts live inside definition/scenario/group/folder directories.
pub fn is_clonk_script(p: &Path) -> bool {
    let is_c = p.extension().map(|e| e.eq_ignore_ascii_case("c")).unwrap_or(false);
    is_c && p.ancestors().skip(1).any(|a| {
        let n = a.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        [".c4d", ".c4s", ".c4g", ".c4f"].iter().any(|e| n.ends_with(e))
    })
}

fn in_system_group(p: &Path) -> bool {
    p.ancestors().skip(1).any(|a| {
        a.file_name().map(|n| n.to_string_lossy().eq_ignore_ascii_case("System.c4g")).unwrap_or(false)
    })
}

fn is_system_path(p: &Path) -> bool {
    in_system_group(p)
}

/// Files of interest found by `collect_dir` (phase 1 of indexing).
#[derive(Default)]
struct Found {
    scripts: Vec<PathBuf>,
    defcores: Vec<PathBuf>,
    scenarios: Vec<PathBuf>,
}

/// Walk `dir` and remember scripts, DefCore.txt and Scenario.txt. Uses the directory entry's file type
/// (no extra stat per entry, which is slow on Windows).
fn collect_dir(dir: &Path, seen: &mut HashSet<String>, found: &mut Found) {
    if !seen.insert(path_key(dir)) {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_dir = match entry.file_type() {
            Ok(t) if t.is_symlink() => entry.path().is_dir(),
            Ok(t) => t.is_dir(),
            Err(_) => false,
        };
        if is_dir {
            if [".git", "target", "node_modules", ".claude"].contains(&name.as_ref()) {
                continue;
            }
            collect_dir(&entry.path(), seen, found);
        } else if name.eq_ignore_ascii_case("DefCore.txt") {
            found.defcores.push(entry.path());
        } else if name.eq_ignore_ascii_case("Scenario.txt") {
            found.scenarios.push(entry.path());
        } else if name.len() > 2 && name[name.len() - 2..].eq_ignore_ascii_case(".c") {
            // the engine only loads Script.c of definitions/scenarios and the *.c files of System.c4g groups
            let p = entry.path();
            if is_clonk_script(&p) && (name.eq_ignore_ascii_case("Script.c") || in_system_group(&p)) {
                found.scripts.push(p);
            }
        }
    }
}

/// Map `f` over `items` on all CPU cores and keep the input order. Threads take the next item from a shared
/// counter, so a few huge files (e.g. 2 MB generated map scripts) don't stall one thread's share.
/// `init` creates per-thread state (e.g. a tree-sitter parser).
fn par_map<T: Sync, S, R: Send>(items: &[T], init: impl Fn() -> S + Sync, f: impl Fn(&mut S, &T) -> Option<R> + Sync) -> Vec<R> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 16).min(items.len().max(1));
    let next = AtomicUsize::new(0);
    let mut results: Vec<(usize, R)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut state = init();
                    let mut out = vec![];
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= items.len() {
                            break;
                        }
                        if let Some(r) = f(&mut state, &items[i]) {
                            out.push((i, r));
                        }
                    }
                    out
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    results.sort_by_key(|(i, _)| *i);
    results.into_iter().map(|(_, r)| r).collect()
}

/// Read, parse and extract the scripts on all CPU cores (one tree-sitter parser per thread).
fn parse_scripts_parallel(paths: &[PathBuf]) -> Vec<(String, FileInfo)> {
    par_map(paths, new_parser, |parser, p| {
        let src = decode_bytes(&std::fs::read(p).ok()?);
        let tree = parser.parse(&src, None)?;
        Some((path_key(p), extract(p, &src, &tree)))
    })
}

/// DefCore.txt facts for the ID table (read in parallel; inserted in walk order by `add_found`).
fn read_defcore(p: &Path) -> Option<IdInfo> {
    let text = decode_bytes(&std::fs::read(p).ok()?);
    let (mut id, mut name, mut line) = (None, None, 0);
    for (i, l) in text.lines().enumerate() {
        let l = l.trim();
        if let Some(v) = l.strip_prefix("id=") {
            id = Some(v.trim().to_string());
            line = i as u32;
        } else if let Some(v) = l.strip_prefix("Name=") {
            name = Some(v.trim().to_string());
        }
    }
    let script = p.parent().map(|d| d.join("Script.c")).filter(|s| s.exists());
    Some(IdInfo { id: id?, name, defcore: p.to_path_buf(), line, script })
}

const EMBEDDED_SYSTEM: &str = include_str!("../core/system_scripts.json");

/// Write the embedded System.c4g scripts (from LegacyClonk/planet) to a temp folder so they can be
/// indexed like real files and navigation can open them.
fn extract_embedded_system_group() -> Option<PathBuf> {
    let scripts: HashMap<String, String> = serde_json::from_str(EMBEDDED_SYSTEM).ok()?;
    let dir = std::env::temp_dir().join("legacy-clonk-ls").join("System.c4g");
    std::fs::create_dir_all(&dir).ok()?;
    for (name, content) in scripts {
        let p = dir.join(name);
        if std::fs::read_to_string(&p).ok().as_deref() != Some(content.as_str()) {
            std::fs::write(&p, content).ok()?;
        }
    }
    Some(dir)
}

pub fn new_parser() -> Parser {
    let mut parser = Parser::new();
    parser.set_language(tree_sitter_c4script::language()).expect("c4script grammar");
    parser
}

/// Children helper (tree-sitter 0.20 has no allocation-free iterator that outlives the cursor).
pub fn children<'t>(n: &Node<'t>) -> Vec<Node<'t>> {
    let mut c = n.walk();
    n.children(&mut c).collect()
}

pub fn named_children<'t>(n: &Node<'t>) -> Vec<Node<'t>> {
    let mut c = n.walk();
    n.named_children(&mut c).collect()
}

/// Is this method_call the right-hand side of `obj->Call()`?
/// A map key, not a variable: `speed` in `{speed = 5}` or in `m.speed` (but not `m[speed]`, an expression).
pub fn is_map_key(node: &Node) -> bool {
    match node.parent() {
        Some(p) if p.kind() == "map_key" => true,
        Some(p) if p.kind() == "map_access" => node.prev_sibling().map(|s| s.kind() == ".").unwrap_or(false),
        _ => false,
    }
}

/// The loop variables of `for ([var] x in a)` / `for ([var] k, v in m)`: the identifiers before `in` (the
/// collection after it can be a plain name too).
pub fn for_in_vars<'t>(n: &Node<'t>) -> Vec<Node<'t>> {
    let kids = children(n);
    let Some(at) = kids.iter().position(|c| c.kind() == "in") else { return vec![] };
    kids[..at].iter().filter(|c| c.kind() == "identifier").copied().collect()
}

/// `&x` / `int &x`: a reference parameter.
pub fn is_ref_param(p: &Node) -> bool {
    children(p).iter().any(|c| c.kind() == "&")
}

pub fn is_arrow_call(call: &Node) -> bool {
    match call.parent() {
        Some(p) if p.kind() == "arrow_expression" => named_children(&p).first().map(|f| f.id() != call.id()).unwrap_or(false),
        _ => false,
    }
}

fn read_def_id(dir: &Path) -> Option<String> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        if entry.file_name().to_string_lossy().eq_ignore_ascii_case("DefCore.txt") {
            let text = decode_bytes(&std::fs::read(entry.path()).ok()?);
            for line in text.lines() {
                if let Some(v) = line.trim().strip_prefix("id=") {
                    return Some(v.trim().to_string());
                }
            }
        }
    }
    None
}

pub fn extract(path: &Path, src: &str, tree: &Tree) -> FileInfo {
    let li = LineIndex::new(src);
    let mut info = FileInfo {
        path: path.to_path_buf(),
        def_id: path.parent().and_then(read_def_id),
        is_system: in_system_group(path),
        includes: vec![],
        appendto: vec![],
        funcs: vec![],
        vars: vec![],
        calls: vec![],
        string_words: HashSet::new(),
        string_prefixes: HashSet::new(),
        own_string_words: HashSet::new(),
        ini_calls: vec![],
        read_idents: HashSet::new(),
        locals_by_index: false,
        id_refs: vec![],
        writes: vec![],
        ref_args: vec![],
    };
    if let Some(dir) = path.parent() {
        info.ini_calls = ini_calls(dir);
    }
    collect_reads(&tree.root_node(), src, &mut info);
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "function_definition" | "old_function_definition" => {
                if let Some(name) = n.child_by_field_name("name") {
                    let mut params = vec![];
                    for c in named_children(&n) {
                        if c.kind() == "parameter_list" {
                            for p in named_children(&c).into_iter().filter(|p| p.kind() == "param") {
                                if let Some(pn) = p.child_by_field_name("name") {
                                    let ty = if is_ref_param(&p) { Some("&".to_string()) } else { p.child_by_field_name("type").map(|t| node_text(&t, src).to_string()) };
                                    params.push((ty, node_text(&pn, src).to_string()));
                                }
                            }
                        }
                    }
                    let (writes, ref_args) = function_writes(&n, src);
                    for (w, member) in writes {
                        info.writes.push((w, info.funcs.len(), member));
                    }
                    for (v, callee, pos) in ref_args {
                        info.ref_args.push((v, info.funcs.len(), callee, pos));
                    }
                    info.funcs.push(FuncDef {
                        name: node_text(&name, src).to_string(),
                        params,
                        visibility: n.child_by_field_name("visibility").map(|v| node_text(&v, src).to_string()),
                        name_range: li.node_range(&name),
                        range: li.node_range(&n),
                        doc: doc_comment(&n, src),
                    });
                }
            }
            "var_definition" if n.parent().map(|p| p.kind() == "source_file").unwrap_or(false) => {
                let scope = named_children(&n).first().map(|s| node_text(s, src).to_string()).unwrap_or_default();
                let is_const = n.child_by_field_name("const").is_some();
                let kind = match (scope.as_str(), is_const) {
                    ("static", true) => VarKind::StaticConst,
                    ("static", false) => VarKind::Static,
                    _ => VarKind::Local,
                };
                for a in named_children(&n).into_iter().filter(|c| c.kind() == "var_assignment") {
                    let parts = named_children(&a);
                    if let Some(id) = parts.first() {
                        info.vars.push(VarDef {
                            name: node_text(id, src).to_string(),
                            kind,
                            range: li.node_range(id),
                            value: parts.get(1).map(|v| node_text(v, src).to_string()),
                        });
                    }
                }
            }
            "include" | "appendto" => {
                let target = named_children(&n).into_iter().find(|c| c.kind() == "id").map(|c| node_text(&c, src).to_string());
                let target = target.unwrap_or_else(|| "*".to_string());
                if n.kind() == "include" { info.includes.push(target) } else { info.appendto.push(target) }
            }
            "method_call" => {
                if let Some(name) = n.child_by_field_name("name") {
                    let args = n.child_by_field_name("args").map(|a| named_children(&a).iter().filter(|c| c.kind() != "comment").count()).unwrap_or(0);
                    info.calls.push(CallSite {
                        name: node_text(&name, src).to_string(),
                        range: li.node_range(&name),
                        byte: name.start_byte(),
                        arrow: is_arrow_call(&n),
                        args,
                    });
                }
            }
            "id" => info.id_refs.push((node_text(&n, src).to_string(), li.node_range(&n))),
            // names in strings may be called by name (Schedule("F()"), Call("F"), menus, commands): callers.rs
            // [Desc|Condition=Func]: the engine evaluates Func on the object (context menu entries)
            "func_desc" => {
                let text = node_text(&n, src);
                for part in text.split('|') {
                    if let Some(f) = part.trim().strip_prefix("Condition=") {
                        info.own_string_words.insert(f.trim_end_matches(']').trim().to_string());
                    }
                }
            }
            "string" if !string_may_call(&n, src) => {}
            "string" => {
                let text = node_text(&n, src);
                if own_object_string(&n, src) {
                    string_words(text, &mut info.own_string_words, &mut info.string_prefixes);
                } else {
                    string_words(text, &mut info.string_words, &mut info.string_prefixes);
                }
            }
            _ => {}
        }
        let mut kids = named_children(&n);
        kids.reverse();
        stack.extend(kids);
    }
    info
}

const ASSIGN_OPS: &[&str] = &["=", "+=", "-=", "*=", "/=", "%=", "**=", "<<=", ">>=", "&=", "|=", "^=", "..=", "??="];

/// Script variables a function assigns: plain names that aren't its parameters/vars (`n = 1`, `n++`, `n[] = x`),
/// and other objects' locals (`obj.n = x`, `LocalN("n", obj) = x`) as member writes.
fn function_writes(f: &Node, src: &str) -> (Vec<(String, bool)>, Vec<(String, String, usize)>) {
    let mut locals = HashSet::new();
    let mut stack = vec![*f];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "param" => locals.extend(n.child_by_field_name("name").map(|x| node_text(&x, src).to_string())),
            "var_assignment" => {
                let is_var = n.parent().map(|p| named_children(&p).iter().any(|c| c.kind() == "var_scope" && node_text(c, src) == "var")).unwrap_or(false);
                if is_var {
                    locals.extend(named_children(&n).first().map(|x| node_text(x, src).to_string()));
                }
            }
            "for_statement" if children(&n).iter().any(|c| c.kind() == "var") => {
                for id in for_in_vars(&n) {
                    locals.insert(node_text(&id, src).to_string());
                }
            }
            _ => {}
        }
        stack.extend(named_children(&n));
    }
    let op = |n: &Node| children(n).iter().find(|c| !c.is_named() && !matches!(c.kind(), "(" | ")")).map(|c| c.kind().to_string()).unwrap_or_default();
    let mut out: Vec<(String, bool)> = vec![];
    let mut push = |name: &str, member: bool| {
        if !out.iter().any(|(n, m)| n == name && *m == member) {
            out.push((name.to_string(), member));
        }
    };
    let mut ref_args: Vec<(String, String, usize)> = vec![];
    let mut stack = vec![*f];
    while let Some(n) = stack.pop() {
        if n.kind() == "method_call" {
            let callee = n.child_by_field_name("name").map(|x| node_text(&x, src).to_string()).unwrap_or_default();
            let args = n.child_by_field_name("args").map(|a| named_children(&a)).unwrap_or_default();
            for (pos, a) in args.iter().filter(|c| c.kind() != "comment").enumerate() {
                let name = node_text(a, src);
                if a.kind() == "identifier" && !locals.contains(name) && name != "this" && !ref_args.iter().any(|(v, c, p)| v == name && *c == callee && *p == pos) {
                    ref_args.push((name.to_string(), callee.clone(), pos));
                }
            }
        }
        let lhs = match n.kind() {
            "binary_expression" if ASSIGN_OPS.contains(&op(&n).as_str()) => named_children(&n).into_iter().find(|c| c.kind() != "comment"),
            "unary_expression" if matches!(op(&n).as_str(), "++" | "--") => named_children(&n).into_iter().find(|c| c.kind() != "comment"),
            _ => None,
        };
        if let Some(l) = lhs {
            let dot = children(&l).iter().any(|c| c.kind() == ".");
            match l.kind() {
                "identifier" => {
                    let name = node_text(&l, src);
                    if !locals.contains(name) && name != "this" {
                        push(name, false);
                    }
                }
                "array_append" | "map_access" if !dot => {
                    if let Some(b) = named_children(&l).into_iter().next().filter(|b| b.kind() == "identifier") {
                        if !locals.contains(node_text(&b, src)) {
                            push(node_text(&b, src), false);
                        }
                    }
                }
                "map_access" => {
                    if let Some(k) = named_children(&l).get(1) {
                        push(node_text(k, src), true);
                    }
                }
                "method_call" if l.child_by_field_name("name").map(|x| node_text(&x, src) == "LocalN").unwrap_or(false) => {
                    let first = l.child_by_field_name("args").and_then(|a| named_children(&a).into_iter().find(|c| c.kind() != "comment"));
                    if let Some(s) = first.filter(|s| s.kind() == "string") {
                        push(node_text(&s, src).trim_matches('"'), true);
                    }
                }
                _ => {}
            }
        }
        stack.extend(named_children(&n));
    }
    (out, ref_args)
}

/// Comment lines directly above a function definition.
fn doc_comment(n: &Node, src: &str) -> Option<String> {
    let mut lines = vec![];
    let mut cur = n.prev_named_sibling();
    let mut next_row = n.start_position().row;
    while let Some(c) = cur {
        if c.kind() != "comment" || c.end_position().row + 1 < next_row {
            break;
        }
        lines.push(node_text(&c, src).to_string());
        next_row = c.start_position().row;
        cur = c.prev_named_sibling();
    }
    if lines.is_empty() {
        return None;
    }
    lines.reverse();
    Some(lines.join("\n"))
}

impl Index {
    pub fn build(roots: Vec<PathBuf>) -> Index {
        let timing = std::env::var_os("C4LS_TIMING").is_some();
        let t0 = std::time::Instant::now();
        let mut index = Index { roots: roots.clone(), ..Default::default() };
        let mut seen = HashSet::new();
        let mut found = Found::default();
        for root in roots {
            collect_dir(&root, &mut seen, &mut found);
        }
        // The engine always loads System.c4g; if the roots don't contain one, use the embedded copy
        if !found.scripts.iter().any(|p| is_system_path(p)) {
            if let Some(dir) = extract_embedded_system_group() {
                collect_dir(&dir, &mut seen, &mut found);
            }
        }
        let t_walk = t0.elapsed();
        let counts = (found.scripts.len(), found.defcores.len(), found.scenarios.len());
        index.add_found(found);
        if timing {
            eprintln!("[timing] index: walk {:?} ({} scripts, {} DefCores, {} scenarios), read+parse+extract {:?}",
                t_walk, counts.0, counts.1, counts.2, t0.elapsed() - t_walk);
        }
        index
    }

    fn scan_dir(&mut self, dir: &Path, _parser: &mut Parser, seen: &mut HashSet<String>) {
        let mut found = Found::default();
        collect_dir(dir, seen, &mut found);
        self.add_found(found);
    }

    fn add_found(&mut self, found: Found) {
        let t = std::time::Instant::now();
        for info in par_map(&found.defcores, || (), |_, p| read_defcore(p)) {
            self.add_id(info);
        }
        for p in &found.scenarios {
            self.collect_definitions(p);
        }
        let t_meta = t.elapsed();
        for (key, info) in parse_scripts_parallel(&found.scripts) {
            self.files.insert(key, info);
        }
        self.lookup = std::sync::OnceLock::new();
        self.scope_cache = Default::default();
        if std::env::var_os("C4LS_TIMING").is_some() {
            eprintln!("[timing] DefCore+Scenario.txt {:?}, scripts {:?}", t_meta, t.elapsed() - t_meta);
        }
    }

    /// Remember the packs a scenario loads ([Definitions] in Scenario.txt, see C4SDefinitions):
    /// `Definition1=Objects.c4d` ... `Definition10=`, or `Definitions=A.c4d;B.c4d`; `LocalOnly=1` loads none.
    /// Without any entry the engine uses the definitions from its configuration; we assume Objects.c4d then.
    fn collect_definitions(&mut self, p: &Path) {
        let text = match std::fs::read(p) {
            Ok(b) => decode_bytes(&b),
            Err(_) => return,
        };
        let mut packs: Vec<String> = vec![];
        let mut local_only = false;
        for line in text.lines() {
            let line = line.trim();
            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim();
                let value = value.trim().replace('\\', "/");
                let numbered = key.len() > 10 && key[..10].eq_ignore_ascii_case("Definition") && key[10..].chars().all(|c| c.is_ascii_digit());
                if numbered && !value.is_empty() {
                    packs.push(value);
                } else if key.eq_ignore_ascii_case("Definitions") {
                    packs.extend(value.split(';').map(|v| v.trim().to_string()).filter(|v| !v.is_empty()));
                } else if key.eq_ignore_ascii_case("LocalOnly") {
                    local_only = value == "1" || value.eq_ignore_ascii_case("true");
                }
            }
        }
        if local_only {
            packs.clear();
        } else if packs.is_empty() {
            packs.push("Objects.c4d".to_string());
        }
        self.referenced_packs.extend(packs.iter().cloned());
        if let Some(dir) = p.parent() {
            let names = packs
                .iter()
                .map(|v| v.rsplit('/').next().unwrap_or(v).to_lowercase())
                .collect();
            self.scenario_packs.insert(format!("{}/", path_key(dir)), names);
        }
    }

    /// Would the engine load `target` (a script or DefCore.txt) when running the scenario `from` belongs to?
    /// Mirrors C4Game::OpenScenario/InitDefs: the scenario's own folder, every enclosing .c4f folder (minus
    /// other scenarios inside them), the packs from its [Definitions], and System.c4g groups. Scripts that
    /// aren't part of a scenario (packs, folder definitions) can be loaded by any scenario: everything is
    /// visible from them.
    pub fn visible_from(&self, from: &Path, target: &Path) -> bool {
        let fk = path_key(from);
        let tk = path_key(target);
        let scen = match prefix_through(&fk, ".c4s", true) {
            Some(s) => s,
            None => return true,
        };
        if tk.starts_with(&scen) {
            return true;
        }
        // local definitions of other scenarios are never loaded
        if prefix_through(&tk, ".c4s", true).is_some() {
            return false;
        }
        let folders = folder_prefixes(&scen);
        if folders.iter().any(|f| tk.starts_with(f.as_str())) {
            return true;
        }
        let default_packs = vec!["objects.c4d".to_string()];
        let packs = self.scenario_packs.get(&scen).unwrap_or(&default_packs);
        if packs.iter().any(|p| tk.contains(&format!("/{}/", p))) {
            return true;
        }
        // System.c4g of the game (not of a scenario folder or a pack: a pack's System.c4g counts only if the
        // scenario loads that pack, checked above)
        tk.contains("/system.c4g/") && prefix_through(&tk, ".c4f", false).is_none() && prefix_through(&tk, ".c4d", false).is_none()
    }

    /// Is every pack that the scenario of `from` loads indexed, so its loaded functions are completely known?
    /// Scripts outside scenarios: unknown which scenario loads them -> false.
    pub fn scope_packs_indexed(&self, from: &Path) -> bool {
        let scen = match prefix_through(&path_key(from), ".c4s", true) {
            Some(s) => s,
            None => return false,
        };
        let default_packs = vec!["objects.c4d".to_string()];
        let packs = self.scenario_packs.get(&scen).unwrap_or(&default_packs);
        let lk = self.lookup();
        packs.iter().all(|p| lk.pack_dirs.contains(&p.rsplit('/').next().unwrap_or(p).to_lowercase()))
    }

    /// Does the scenario of script `from` load the pack with this file name (e.g. "Knights.c4d")?
    /// Scripts outside scenarios: always.
    pub fn loads_pack(&self, from: &Path, pack: &str) -> bool {
        let scen = match prefix_through(&path_key(from), ".c4s", true) {
            Some(s) => s,
            None => return true,
        };
        let name = pack.rsplit('/').next().unwrap_or(pack).to_lowercase();
        match self.scenario_packs.get(&scen) {
            Some(packs) => packs.contains(&name),
            None => name == "objects.c4d",
        }
    }

    /// First definition with this ID that the script `from` can see.
    /// The definition with this ID a script sees. Several may be visible: the engine loads the packs first, then the
    /// .c4f folders (outer to inner), then the scenario's own definitions, and a later one with the same ID replaces
    /// the earlier (C4DefList::Add with overload). So the most local visible definition wins.
    pub fn id_for(&self, id: &str, from: &Path) -> Option<&IdInfo> {
        let fk = path_key(from);
        let scen = prefix_through(&fk, ".c4s", true);
        let rank = |i: &IdInfo| -> usize {
            let dk = path_key(&i.defcore);
            match &scen {
                Some(s) if dk.starts_with(s.as_str()) => usize::MAX,
                Some(s) => folder_prefixes(s).iter().filter(|f| dk.starts_with(f.as_str())).map(|f| f.len()).max().unwrap_or(0),
                None => 0,
            }
        };
        let mut best: Option<(&IdInfo, usize)> = None;
        for i in self.ids.get(id)?.iter().filter(|i| self.visible_from(from, &i.defcore)) {
            let r = rank(i);
            if best.map_or(true, |(_, b)| r > b) {
                best = Some((i, r));
            }
        }
        best.map(|(i, _)| i)
    }

    /// Any definition with this ID (e.g. for hovers in pack scripts).
    pub fn id_any(&self, id: &str) -> Option<&IdInfo> {
        self.ids.get(id)?.first()
    }

    /// Make sure the Clonk folder around `file` is indexed, even if it lies outside the roots (clients may pass
    /// a different workspace root, or none). Indexes the outermost enclosing .c4s/.c4d/.c4f/.c4g folder.
    /// Returns that folder if it was newly indexed.
    pub fn cover(&mut self, file: &Path) -> Option<PathBuf> {
        let outermost = file
            .ancestors()
            .skip(1)
            .filter(|a| {
                let n = a.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
                [".c4s", ".c4d", ".c4f", ".c4g"].iter().any(|e| n.ends_with(e))
            })
            .last()?
            .to_path_buf();
        let key = path_key(&outermost);
        let inside = |base: &PathBuf| key.starts_with(&path_key(base));
        if self.roots.iter().any(inside) || self.covered.iter().any(inside) {
            return None;
        }
        let mut parser = new_parser();
        let mut seen = HashSet::new();
        self.scan_dir(&outermost, &mut parser, &mut seen);
        self.covered.push(outermost.clone());
        Some(outermost)
    }

    /// Merge another index (e.g. of game packs); existing entries win.
    pub fn merge(&mut self, other: Index) {
        for (k, f) in other.files {
            self.files.entry(k).or_insert(f);
        }
        self.lookup = std::sync::OnceLock::new();
        self.scope_cache = Default::default();
        for (k, v) in other.ids {
            let list = self.ids.entry(k).or_default();
            for i in v {
                if !list.iter().any(|x| path_key(&x.defcore) == path_key(&i.defcore)) {
                    list.push(i);
                }
            }
        }
        for (k, v) in other.scenario_packs {
            self.scenario_packs.entry(k).or_insert(v);
        }
        self.covered.extend(other.covered);
    }

    /// Is a pack with this file name (e.g. "Objects.c4d") already indexed as a folder somewhere?
    pub fn has_pack_folder(&self, pack: &str) -> bool {
        let needle = format!("/{}/", pack.to_lowercase());
        self.files.keys().any(|k| k.contains(&needle))
    }

    fn add_id(&mut self, info: IdInfo) {
        let list = self.ids.entry(info.id.clone()).or_default();
        let key = path_key(&info.defcore);
        if !list.iter().any(|x| path_key(&x.defcore) == key) {
            // workspace roots are scanned first, so their definitions come first
            list.push(info);
        }
    }

    pub fn update_file(&mut self, info: FileInfo) {
        self.files.insert(path_key(&info.path), info);
        self.lookup = std::sync::OnceLock::new();
        self.scope_cache = Default::default();
    }

    fn lookup(&self) -> &Lookup {
        self.lookup.get_or_init(|| Lookup::build(&self.files))
    }

    /// (file, item index) pairs of a lookup table entry
    fn resolve<'a>(&'a self, entries: Option<&'a Vec<(String, usize)>>) -> impl Iterator<Item = (&'a FileInfo, usize)> + 'a {
        entries.into_iter().flatten().filter_map(move |(k, i)| self.files.get(k).map(|f| (f, *i)))
    }

    /// Does `info` #include/#appendto a definition whose script isn't indexed (e.g. from a pack we don't have)?
    /// Then its visible functions and locals are unknown.
    pub fn has_unresolved_targets(&self, info: &FileInfo) -> bool {
        info.includes.iter().chain(info.appendto.iter().filter(|t| t.as_str() != "*"))
            .any(|id| self.script_of(id, &info.path).is_none() && self.id_for(id, &info.path).map_or(true, |i| i.script.is_some()))
    }

    /// Scripts that #include the definition `id` (directly).
    pub fn includers_of(&self, id: &str) -> Vec<&FileInfo> {
        self.lookup().includers.get(id).into_iter().flatten().filter_map(|k| self.files.get(k)).collect()
    }

    /// Scripts that #appendto the definition `id` (or every definition: `#appendto *`).
    pub fn appenders_of(&self, id: &str) -> Vec<&FileInfo> {
        let lk = self.lookup();
        lk.appenders.get(id).into_iter().flatten().chain(lk.appenders.get("*").into_iter().flatten())
            .filter_map(|k| self.files.get(k))
            .collect()
    }

    pub fn script_of(&self, id: &str, from: &Path) -> Option<&FileInfo> {
        let script = self.id_for(id, from)?.script.as_ref()?;
        self.files.get(&path_key(script))
    }

    /// Files whose functions/locals are visible from `info`: itself, its #include chain,
    /// #appendto scripts of those definitions, and (for an #appendto script) its target.
    pub fn scope_files<'a>(&'a self, info: &'a FileInfo) -> Vec<&'a FileInfo> {
        // the open document may differ from its indexed version: its own #include/#appendto lines are in the key
        let key = format!("{}|{}|{}", path_key(&info.path), info.includes.join(","), info.appendto.join(","));
        if let Some(keys) = self.scope_cache.lock().ok().and_then(|c| c.get(&key).cloned()) {
            let mut out = vec![info];
            out.extend(keys.iter().filter_map(|k| self.files.get(k)));
            return out;
        }
        let out = self.scope_files_uncached(info);
        if let Ok(mut c) = self.scope_cache.lock() {
            c.insert(key, out.iter().skip(1).map(|f| path_key(&f.path)).collect());
        }
        out
    }

    fn scope_files_uncached<'a>(&'a self, info: &'a FileInfo) -> Vec<&'a FileInfo> {
        let mut out: Vec<&FileInfo> = vec![];
        let mut seen = HashSet::new();
        let mut todo = vec![info];
        while let Some(f) = todo.pop() {
            if !seen.insert(path_key(&f.path)) {
                continue;
            }
            out.push(f);
            let mut ids: Vec<&str> = f.includes.iter().map(|s| s.as_str()).collect();
            ids.extend(f.appendto.iter().filter(|t| t.as_str() != "*").map(|s| s.as_str()));
            for id in ids {
                if let Some(s) = self.script_of(id, &info.path) {
                    todo.push(s);
                }
            }
            if let Some(own) = &f.def_id {
                let lk = self.lookup();
                for key in lk.appenders.get(own).into_iter().flatten().chain(lk.appenders.get("*").into_iter().flatten()) {
                    if let Some(other) = self.files.get(key) {
                        if self.visible_from(&info.path, &other.path) {
                            todo.push(other);
                        }
                    }
                }
            }
        }
        out
    }

    /// Functions assigning the script variable `name`: (script, function index, member write).
    /// Every call of a function named `name` (direct and obj->name()) in the indexed scripts.
    pub fn call_sites<'a>(&'a self, name: &str) -> Vec<(&'a FileInfo, &'a CallSite)> {
        self.lookup().call_sites.get(name).into_iter().flatten()
            .filter_map(|(k, i)| self.files.get(k).and_then(|f| f.calls.get(*i).map(|c| (f, c))))
            .collect()
    }

    /// Whether the function `name` of `file` may be called by name from a string: one that reaches any object
    /// (GameCall, ObjectCall, "obj->F()", menus, effects, ...) anywhere, an own-object call (Schedule("F()"),
    /// ScheduleCall(this, "F"), ...) in its definition family, or a Format("Name%d") prefix.
    pub fn named_in_strings(&self, file: &FileInfo, name: &str) -> bool {
        self.named_in_strings_of(file, None, name)
    }

    /// named_in_strings with the family of `file` already at hand.
    pub fn named_in_strings_of(&self, file: &FileInfo, family: Option<&[&FileInfo]>, name: &str) -> bool {
        let l = self.lookup();
        if l.string_words.contains(name) || l.string_prefixes.iter().any(|p| p.len() >= 3 && name.starts_with(p.as_str())) {
            return true;
        }
        // Fire1, Fire2: the name may be built from "Fire" and a number at runtime
        let base = name.trim_end_matches(|c: char| c.is_ascii_digit());
        if base.len() >= 3 && base.len() < name.len() && (l.string_words.contains(base) || l.read_counts.contains_key(base)) {
            return true;
        }
        if file.own_string_words.contains(name) {
            return true;
        }
        match family {
            Some(fam) => fam.iter().any(|f| f.own_string_words.contains(name)),
            None => self.family(file).iter().any(|f| f.own_string_words.contains(name)),
        }
    }

    /// How many indexed scripts read a variable named `name` (or name it in a LocalN / may-run string).
    pub fn read_count(&self, name: &str) -> u32 {
        self.lookup().read_counts.get(name).copied().unwrap_or(0)
    }

    /// How ActMap/DefCore files call a function: (script next to the file, key like "PhaseCall", procedure).
    pub fn ini_uses(&self, name: &str) -> Vec<(String, String, String)> {
        self.lookup().ini_calls.get(name).cloned().unwrap_or_default()
    }

    /// Whether some script or engine function `callee` has a reference parameter at `pos`.
    pub fn is_ref_param(&self, callee: &str, pos: usize) -> bool {
        self.lookup().ref_params.contains(&(callee.to_string(), pos))
    }

    /// Functions of `file` that pass the script variable `name` to a reference parameter (write sites; `file`'s own
    /// functions count too: an open document may not be in the index).
    pub fn ref_writes<'a>(&self, file: &'a FileInfo, name: &str) -> Vec<usize> {
        let mut out: Vec<usize> = file.ref_args.iter().filter(|(v, _, c, p)| v == name && (self.is_ref_param(c, *p)
            || file.funcs.iter().any(|f| f.name == *c && f.params.get(*p).and_then(|x| x.0.as_deref()) == Some("&")))).map(|(_, i, _, _)| *i).collect();
        out.dedup();
        out
    }

    pub fn writers<'a>(&'a self, name: &str) -> Vec<(&'a FileInfo, usize, bool)> {
        self.lookup().writers.get(name).into_iter().flatten()
            .filter_map(|(k, i, m)| self.files.get(k).map(|f| (f, *i, *m)))
            .collect()
    }

    /// Every script whose objects share `def`'s locals: its scope (#include chain, #appendto) and the scripts that
    /// #include or #appendto it (transitively).
    pub fn family<'a>(&'a self, def: &'a FileInfo) -> Vec<&'a FileInfo> {
        let mut out = self.scope_files(def);
        let mut seen: HashSet<String> = out.iter().map(|f| path_key(&f.path)).collect();
        let mut todo: Vec<String> = def.def_id.iter().cloned().collect();
        let mut seen_ids = HashSet::new();
        while let Some(id) = todo.pop() {
            if !seen_ids.insert(id.clone()) {
                continue;
            }
            let lk = self.lookup();
            for key in lk.includers.get(&id).into_iter().flatten().chain(lk.appenders.get(&id).into_iter().flatten()) {
                if let Some(f) = self.files.get(key) {
                    if seen.insert(key.clone()) {
                        out.push(f);
                    }
                    if let Some(i) = &f.def_id {
                        todo.push(i.clone());
                    }
                }
            }
        }
        out
    }

    /// Global functions named `name` that the script `from` can see.
    pub fn global_funcs<'a>(&'a self, from: &Path, name: &str) -> Vec<(&'a FileInfo, &'a FuncDef)> {
        self.resolve(self.lookup().globals.get(name))
            .filter(|(f, _)| self.visible_from(from, &f.path))
            .map(|(f, i)| (f, &f.funcs[i]))
            .collect()
    }

    /// Functions named `name` in any script the script `from` can see (targets of `obj->Name()` calls).
    pub fn reachable_funcs<'a>(&'a self, from: &Path, name: &str) -> Vec<(&'a FileInfo, &'a FuncDef)> {
        self.all_funcs(name).into_iter().filter(|(f, _)| self.visible_from(from, &f.path)).collect()
    }

    pub fn all_funcs<'a>(&'a self, name: &str) -> Vec<(&'a FileInfo, &'a FuncDef)> {
        self.resolve(self.lookup().funcs.get(name)).map(|(f, i)| (f, &f.funcs[i])).collect()
    }

    /// Functions named `name` that a direct call from `info` would reach (engine functions excluded).
    pub fn visible_funcs<'a>(&'a self, info: &'a FileInfo, name: &str) -> Vec<(&'a FileInfo, &'a FuncDef)> {
        let mut out = vec![];
        for f in self.scope_files(info) {
            for func in &f.funcs {
                if func.name == name {
                    out.push((f, func));
                }
            }
        }
        for g in self.global_funcs(&info.path, name) {
            if !out.iter().any(|(f, d)| path_key(&f.path) == path_key(&g.0.path) && d.name_range == g.1.name_range) {
                out.push(g);
            }
        }
        out
    }

    /// Script-level variables named `name` visible from `info`: locals/statics of the scope files,
    /// plus statics/static consts of all scripts (they are global).
    pub fn visible_vars<'a>(&'a self, info: &'a FileInfo, name: &str) -> Vec<(&'a FileInfo, &'a VarDef)> {
        let mut out = vec![];
        for f in self.scope_files(info) {
            for v in &f.vars {
                if v.name == name {
                    out.push((f, v));
                }
            }
        }
        if out.is_empty() {
            for (f, i) in self.resolve(self.lookup().statics.get(name)) {
                if self.visible_from(&info.path, &f.path) {
                    out.push((f, &f.vars[i]));
                }
            }
        }
        out
    }
}

/// Packs of the game folder to index for a workspace: Objects.c4d (loaded by default) plus every pack a
/// scenario names in its [Definitions] section, or all *.c4d/*.c4f with `all`. Packs already present as
/// folders in the workspace (e.g. reference/Objects.c4d) are skipped.
pub fn game_packs_to_index(index: &Index, game_dir: &Path, all: bool) -> Vec<String> {
    let mut packs: Vec<String> = vec!["Objects.c4d".to_string()];
    if all {
        if let Ok(entries) = std::fs::read_dir(game_dir) {
            for e in entries.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                let l = n.to_lowercase();
                if l.ends_with(".c4d") || l.ends_with(".c4f") {
                    packs.push(n);
                }
            }
        }
    } else {
        packs.extend(index.referenced_packs.iter().cloned());
    }
    packs.sort_by_key(|p| p.to_lowercase());
    packs.dedup_by_key(|p| p.to_lowercase());
    packs
        .into_iter()
        .filter(|p| {
            let name = Path::new(p).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            !index.has_pack_folder(&name)
        })
        .collect()
}

/// Index the given packs of the game folder. Packed groups are unpacked once with the game's c4group into a
/// cache folder (keyed by size and modification time, so an updated pack is unpacked again); the originals
/// are never modified. Returns the new index and log lines.
pub fn index_game_packs(game_dir: &Path, packs: &[String]) -> (Index, Vec<String>) {
    let mut index = Index::default();
    let mut log = vec![];
    let mut parser = new_parser();
    let mut seen = HashSet::new();
    for pack in packs {
        let path = game_dir.join(pack);
        if !path.exists() {
            log.push(format!("pack not found in game folder: {}", path.display()));
            continue;
        }
        let dir = if path.is_dir() {
            Some(path.clone())
        } else {
            match unpack_cached(game_dir, &path) {
                Ok(d) => Some(d),
                Err(e) => {
                    log.push(format!("could not unpack {}: {}", path.display(), e));
                    None
                }
            }
        };
        if let Some(dir) = dir {
            let before = index.files.len();
            index.scan_dir(&dir, &mut parser, &mut seen);
            index.covered.push(dir.clone());
            log.push(format!("indexed pack {} ({} scripts) from {}", pack, index.files.len() - before, dir.display()));
        }
    }
    (index, log)
}

pub fn pack_cache_root() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("legacy-clonk-ls").join("packs")
}

fn unpack_cached(game_dir: &Path, pack: &Path) -> Result<PathBuf, String> {
    let meta = std::fs::metadata(pack).map_err(|e| e.to_string())?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = pack.file_name().ok_or("no file name")?.to_string_lossy().to_string();
    let slot = pack_cache_root().join(format!("{}-{}-{}", name, meta.len(), mtime));
    let target = slot.join(&name);
    if target.is_dir() {
        return Ok(target);
    }
    let c4group = ["c4group.exe", "c4group"].iter().map(|n| game_dir.join(n)).find(|p| p.exists()).ok_or("c4group not found in game folder")?;
    std::fs::create_dir_all(&slot).map_err(|e| e.to_string())?;
    std::fs::copy(pack, &target).map_err(|e| e.to_string())?;
    // explode the copy in place (recursively unpacks nested groups)
    let status = std::process::Command::new(&c4group)
        .arg(&target)
        .arg("-x")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() || !target.is_dir() {
        let _ = std::fs::remove_dir_all(&slot);
        return Err(format!("c4group failed ({})", status));
    }
    Ok(target)
}

/// Key prefix up to and including the first (outermost) path segment with this extension, with trailing '/'.
/// With `dir_only`, the segment must be a folder (not the last segment).
fn prefix_through(key: &str, ext: &str, dir_only: bool) -> Option<String> {
    let segs: Vec<&str> = key.split('/').collect();
    let n = if dir_only { segs.len().saturating_sub(1) } else { segs.len() };
    segs[..n].iter().position(|s| s.ends_with(ext)).map(|i| format!("{}/", segs[..=i].join("/")))
}

/// Prefixes (with trailing '/') of all .c4f folders a scenario folder key lies in.
fn folder_prefixes(scenario: &str) -> Vec<String> {
    let segs: Vec<&str> = scenario.trim_end_matches('/').split('/').collect();
    (0..segs.len()).filter(|&i| segs[i].ends_with(".c4f")).map(|i| format!("{}/", segs[..=i].join("/"))).collect()
}
