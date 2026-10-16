//! Workspace index: functions, variables, calls and definition IDs of all scripts below the roots.

use super::text::{decode_bytes, node_text, LineIndex};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tower_lsp::lsp_types::{Range, Url};
use tree_sitter::{Node, Parser, Tree};

#[derive(Debug, Clone)]
pub struct FuncDef {
    pub name: String,
    /// (type, name)
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
    pub ids: HashMap<String, IdInfo>,
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
        id_refs: vec![],
    };
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "function_definition" => {
                if let Some(name) = n.child_by_field_name("name") {
                    let mut params = vec![];
                    for c in named_children(&n) {
                        if c.kind() == "parameter_list" {
                            for p in named_children(&c).into_iter().filter(|p| p.kind() == "param") {
                                if let Some(pn) = p.child_by_field_name("name") {
                                    let ty = p.child_by_field_name("type").map(|t| node_text(&t, src).to_string());
                                    params.push((ty, node_text(&pn, src).to_string()));
                                }
                            }
                        }
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
                        arrow: is_arrow_call(&n),
                        args,
                    });
                }
            }
            "id" => info.id_refs.push((node_text(&n, src).to_string(), li.node_range(&n))),
            _ => {}
        }
        let mut kids = named_children(&n);
        kids.reverse();
        stack.extend(kids);
    }
    info
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
        let mut index = Index { roots: roots.clone(), ..Default::default() };
        let mut parser = new_parser();
        let mut seen = HashSet::new();
        for root in roots {
            index.scan_dir(&root, &mut parser, &mut seen);
        }
        // The engine always loads System.c4g; if the roots don't contain one, use the embedded copy
        if !index.files.values().any(|f| f.is_system) {
            if let Some(dir) = extract_embedded_system_group() {
                index.scan_dir(&dir, &mut parser, &mut seen);
            }
        }
        index
    }

    fn scan_dir(&mut self, dir: &Path, parser: &mut Parser, seen: &mut HashSet<String>) {
        let key = path_key(dir);
        if !seen.insert(key) {
            return;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if p.is_dir() {
                if [".git", "target", "node_modules", ".claude"].contains(&name.as_str()) {
                    continue;
                }
                self.scan_dir(&p, parser, seen);
            } else if name.eq_ignore_ascii_case("DefCore.txt") {
                self.index_defcore(&p);
            } else if is_clonk_script(&p) {
                if let Ok(bytes) = std::fs::read(&p) {
                    let src = decode_bytes(&bytes);
                    if let Some(tree) = parser.parse(&src, None) {
                        self.files.insert(path_key(&p), extract(&p, &src, &tree));
                    }
                }
            }
        }
    }

    fn index_defcore(&mut self, p: &Path) {
        let text = match std::fs::read(p) {
            Ok(b) => decode_bytes(&b),
            Err(_) => return,
        };
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
        if let Some(id) = id {
            let script = p.parent().map(|d| d.join("Script.c")).filter(|s| s.exists());
            // first one wins (workspace roots are scanned first)
            self.ids.entry(id.clone()).or_insert(IdInfo { id, name, defcore: p.to_path_buf(), line, script });
        }
    }

    pub fn update_file(&mut self, info: FileInfo) {
        self.files.insert(path_key(&info.path), info);
    }

    fn script_of(&self, id: &str) -> Option<&FileInfo> {
        let script = self.ids.get(id)?.script.as_ref()?;
        self.files.get(&path_key(script))
    }

    /// Files whose functions/locals are visible from `info`: itself, its #include chain,
    /// #appendto scripts of those definitions, and (for an #appendto script) its target.
    pub fn scope_files<'a>(&'a self, info: &'a FileInfo) -> Vec<&'a FileInfo> {
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
                if let Some(s) = self.script_of(id) {
                    todo.push(s);
                }
            }
            if let Some(own) = &f.def_id {
                for other in self.files.values() {
                    if other.appendto.iter().any(|t| t == own || (t == "*" && f.def_id.is_some())) {
                        todo.push(other);
                    }
                }
            }
        }
        out
    }

    pub fn global_funcs<'a>(&'a self, name: &str) -> Vec<(&'a FileInfo, &'a FuncDef)> {
        let mut out = vec![];
        for f in self.files.values() {
            for func in &f.funcs {
                if func.name == name && (f.is_system || func.visibility.as_deref() == Some("global")) {
                    out.push((f, func));
                }
            }
        }
        out
    }

    pub fn all_funcs<'a>(&'a self, name: &str) -> Vec<(&'a FileInfo, &'a FuncDef)> {
        let mut out = vec![];
        for f in self.files.values() {
            for func in &f.funcs {
                if func.name == name {
                    out.push((f, func));
                }
            }
        }
        out
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
        for g in self.global_funcs(name) {
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
            for f in self.files.values() {
                for v in &f.vars {
                    if v.name == name && v.kind != VarKind::Local {
                        out.push((f, v));
                    }
                }
            }
        }
        out
    }
}
