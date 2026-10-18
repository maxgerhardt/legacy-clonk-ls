//! Code navigation for C4Script: hover, go to definition, references, document/workspace symbols.

use super::engine::ENGINE;
use super::index::{is_arrow_call, is_map_key, path_key, path_to_uri, FileInfo, FuncDef, Index, VarKind};
use super::text::{node_text, LineIndex};
use tower_lsp::lsp_types::{
    DocumentSymbol, Location, Position, Range, SymbolInformation, SymbolKind,
};
use tree_sitter::{Node, Tree};

/// What the cursor is on.
pub enum Target<'t> {
    /// function name in a call; `arrow` = called via ->
    Call { name: String, arrow: bool, node: Node<'t> },
    FuncDef { name: String },
    Id { id: String },
    Ident { name: String, node: Node<'t> },
    /// key of a map literal or `m.key` access (a name, never a variable)
    MapKey { name: String, node: Node<'t> },
}

pub fn target_at<'t>(tree: &'t Tree, src: &str, li: &LineIndex, pos: Position) -> Option<Target<'t>> {
    let p = li.to_point(pos);
    let n = tree.root_node().named_descendant_for_point_range(p, p)?;
    let text = node_text(&n, src).to_string();
    match n.kind() {
        "id" => Some(Target::Id { id: text }),
        "identifier" if is_map_key(&n) => Some(Target::MapKey { name: text, node: n }),
        "identifier" => {
            let parent = n.parent()?;
            match parent.kind() {
                "method_call" if parent.child_by_field_name("name").map(|x| x.id()) == Some(n.id()) => {
                    Some(Target::Call { name: text, arrow: is_arrow_call(&parent), node: parent })
                }
                "function_definition" | "old_function_definition" if parent.child_by_field_name("name").map(|x| x.id()) == Some(n.id()) => {
                    Some(Target::FuncDef { name: text })
                }
                _ => Some(Target::Ident { name: text, node: n }),
            }
        }
        _ => None,
    }
}

fn loc(path: &std::path::Path, range: Range) -> Option<Location> {
    Some(Location::new(path_to_uri(path)?, range))
}

pub fn func_markdown(file: &FileInfo, f: &FuncDef, index: &Index) -> String {
    let mut s = format!("```c4script\n{}\n```", f.signature());
    let owner = match &file.def_id {
        Some(id) => match index.id_any(id).and_then(|i| i.name.clone()) {
            Some(n) => format!("{} ({})", id, n),
            None => id.clone(),
        },
        None => "scenario/system script".to_string(),
    };
    s.push_str(&format!("\n*{}* - `{}:{}`", owner, file.path.display(), f.name_range.start.line + 1));
    if let Some(d) = &f.doc {
        s.push_str(&format!("\n\n```c4script\n{}\n```", d));
    }
    s
}

/// Script functions for a call: visible ones first; for -> calls (or if none visible) all definitions.
fn call_targets<'a>(index: &'a Index, info: &'a FileInfo, name: &str, arrow: bool) -> Vec<(&'a FileInfo, &'a FuncDef)> {
    let visible = index.visible_funcs(info, name);
    if !arrow && !visible.is_empty() {
        return visible;
    }
    // a direct call that no visible script defines goes to the engine function: don't offer unrelated scripts
    // (e.g. a System.c4g overload of another pack this scenario doesn't load)
    if !arrow && ENGINE.functions.contains_key(name) {
        return Vec::new();
    }
    let mut all = index.reachable_funcs(&info.path, name);
    if all.is_empty() {
        all = index.all_funcs(name);
    }
    all.sort_by_key(|(f, _)| path_key(&f.path));
    all
}

pub fn hover(index: &Index, info: &FileInfo, tree: &Tree, src: &str, pos: Position) -> Option<String> {
    let li = LineIndex::new(src);
    if let Some((name, _, _)) = super::strings::ref_at(src, li.to_byte(pos)) {
        return Some(super::strings::hover(&info.path, &name));
    }
    match target_at(tree, src, &li, pos)? {
        Target::Call { name, arrow, node } => {
            let defs = call_targets(index, info, &name, arrow);
            let mut parts: Vec<String> = defs.iter().take(5).map(|(f, d)| func_markdown(f, d, index)).collect();
            if defs.len() > 5 {
                parts.push(format!("*... and {} more definitions*", defs.len() - 5));
            }
            if parts.is_empty() || arrow {
                if let Some(ef) = ENGINE.functions.get(&name) {
                    parts.push(ef.hover_markdown(&name));
                }
            }
            if parts.is_empty() {
                return None;
            }
            if super::diagnostics::type_checks_enabled() {
                if let Some(t) = super::types::type_at(index, info, src, &tree.root_node(), &node) {
                    parts.push(format!("returns `{}` here *(inferred)*", super::types::display(t)));
                }
            }
            Some(parts.join("\n\n---\n\n"))
        }
        Target::FuncDef { name } => {
            let mut s = String::new();
            if super::diagnostics::type_checks_enabled() {
                if let Some(f) = info.funcs.iter().find(|f| f.name == name) {
                    s = format!("returns `{}` *(inferred from its return statements)*", super::types::display(super::types::script_return(index, info, f)));
                }
            }
            if let Some(ef) = ENGINE.functions.get(&name) {
                s = format!("Overrides/shadows the engine function:\n\n{}", ef.hover_markdown(&name));
            }
            let others: Vec<_> = index.all_funcs(&name).into_iter().filter(|(f, _)| path_key(&f.path) != path_key(&info.path)).collect();
            if !others.is_empty() {
                if !s.is_empty() {
                    s.push_str("\n\n---\n\n");
                }
                s.push_str(&format!("Also defined in {} other script(s):", others.len()));
                for (f, d) in others.iter().take(8) {
                    s.push_str(&format!("\n- `{}:{}`", f.path.display(), d.name_range.start.line + 1));
                }
            }
            if s.is_empty() { None } else { Some(s) }
        }
        Target::Id { id } => {
            let i = match index.id_for(&id, &info.path).or_else(|| index.id_any(&id)) {
                Some(i) => i,
                None => {
                    let std = super::diagnostics::standard_id(&id)?;
                    return Some(format!("**{}** - {}

standard definition from `{}`", id, std.name, std.tlc));
                }
            };
            let mut s = format!("**{}**", id);
            if let Some(n) = &i.name {
                s.push_str(&format!(" - {}", n));
            }
            s.push_str(&format!("\n\n`{}`", i.defcore.parent().unwrap_or(&i.defcore).display()));
            if let Some(script) = &i.script {
                if let Some(f) = index.files.get(&path_key(script)) {
                    let names: Vec<_> = f.funcs.iter().map(|x| x.name.as_str()).collect();
                    if !f.includes.is_empty() {
                        s.push_str(&format!("\n\n#include {}", f.includes.join(", ")));
                    }
                    s.push_str(&format!("\n\nFunctions: {}", names.join(", ")));
                }
            }
            Some(s)
        }
        Target::MapKey { name, node } => {
            let p = node.parent()?;
            // the value: {key = value} entry, or the m.key expression itself
            let (value, what) = if p.kind() == "map_key" {
                (p.next_named_sibling(), "map key".to_string())
            } else {
                let base = p.named_child(0).map(|b| node_text(&b, src).to_string()).unwrap_or_default();
                (Some(p), format!("key of the map `{}`", base))
            };
            let ty = if super::diagnostics::type_checks_enabled() {
                value.and_then(|v| super::types::type_at(index, info, src, &tree.root_node(), &v))
                    .filter(|t| *t != super::types::ANY)
                    .map(|t| format!(": {}", super::types::display(t)))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let keys = map_keys(&enclosing_function(&node).unwrap_or(tree.root_node()), &name, src, &li);
            let lines: Vec<String> = keys.iter().map(|r| (r.start.line + 1).to_string()).collect();
            let at = if lines.is_empty() { String::new() } else { format!(", in map literals on line {}", lines.join(", ")) };
            Some(format!("```c4script
{}{}
```
{}{}", name, ty, what, at))
        }
        Target::Ident { name, node } => {
            if let Some(c) = ENGINE.constants.get(&name) {
                return Some(format!("```c4script\nconst {} {}\n```\n*engine constant*", c.ty, name));
            }
            if let Some(func) = enclosing_function(&node) {
                if let Some(r) = local_declaration(&func, &name, src, &li) {
                    let ty = if super::diagnostics::type_checks_enabled() {
                        super::types::type_at(index, info, src, &tree.root_node(), &node)
                            .map(|t| format!(": {}", super::types::display(t)))
                            .unwrap_or_default()
                    } else {
                        String::new()
                    };
                    let note = if ty.is_empty() { "" } else { " - type at this point *(inferred)*" };
                    return Some(format!("```c4script\nvar {}{}\n```\nlocal variable/parameter, line {}{}", name, ty, r.start.line + 1, note));
                }
            }
            let vars = index.visible_vars(info, &name);
            let (f, v) = vars.first()?;
            let kind = match v.kind {
                VarKind::Local => "local",
                VarKind::Static => "static",
                VarKind::StaticConst => "static const",
            };
            let value = v.value.as_ref().map(|x| format!(" = {}", x)).unwrap_or_default();
            // inferred from all assignments (types.rs field_type)
            let (ty, note) = match super::types::field_type(index, info, src, &name) {
                Some(ft) if v.kind == VarKind::StaticConst || ft.ty == super::types::ANY => (String::new(), String::new()),
                Some(ft) if ft.writers == 0 => (": nil".to_string(), "\n\nnever assigned in the indexed scripts *(inferred)*".to_string()),
                Some(ft) => (
                    format!(": {}", super::types::display(ft.ty)),
                    format!("\n\nfrom {} assigning function(s) in {} script(s); nil until first assigned *(inferred)*", ft.writers, ft.scripts),
                ),
                None => (String::new(), String::new()),
            };
            Some(format!("```c4script\n{} {}{}{}\n```\n`{}:{}`{}", kind, name, ty, value, f.path.display(), v.range.start.line + 1, note))
        }
    }
}

pub fn definition(index: &Index, info: &FileInfo, tree: &Tree, src: &str, pos: Position) -> Vec<Location> {
    let li = LineIndex::new(src);
    // $Name$: the entry in every string table
    if let Some((name, _, _)) = super::strings::ref_at(src, li.to_byte(pos)) {
        return super::strings::tables_for(&info.path).iter()
            .filter_map(|t| t.entries.get(&name).and_then(|(_, line)| loc(&t.path, Range::new(Position::new(*line, 0), Position::new(*line, name.len() as u32)))))
            .collect();
    }
    let target = match target_at(tree, src, &li, pos) {
        Some(t) => t,
        None => return vec![],
    };
    match target {
        Target::Call { name, arrow, .. } => call_targets(index, info, &name, arrow)
            .into_iter()
            .filter_map(|(f, d)| loc(&f.path, d.name_range))
            .collect(),
        Target::FuncDef { name } => index.all_funcs(&name).into_iter().filter_map(|(f, d)| loc(&f.path, d.name_range)).collect(),
        Target::Id { id } => {
            let mut out = vec![];
            if let Some(i) = index.id_for(&id, &info.path).or_else(|| index.id_any(&id)) {
                if let Some(s) = &i.script {
                    out.extend(loc(s, Range::default()));
                }
                let p = Position::new(i.line, 0);
                out.extend(loc(&i.defcore, Range::new(p, p)));
            }
            out
        }
        Target::MapKey { name, node } => {
            map_keys(&enclosing_function(&node).unwrap_or(tree.root_node()), &name, src, &li)
                .into_iter().filter_map(|r| loc(&info.path, r)).collect()
        }
        Target::Ident { name, node } => {
            if let Some(func) = enclosing_function(&node) {
                if let Some(r) = local_declaration(&func, &name, src, &li) {
                    return loc(&info.path, r).into_iter().collect();
                }
            }
            index.visible_vars(info, &name).into_iter().filter_map(|(f, v)| loc(&f.path, v.range)).collect()
        }
    }
}

pub fn references(index: &Index, info: &FileInfo, tree: &Tree, src: &str, pos: Position, include_decl: bool) -> Vec<Location> {
    let li = LineIndex::new(src);
    let target = match target_at(tree, src, &li, pos) {
        Some(t) => t,
        None => return vec![],
    };
    // the open document's fresh info replaces its (possibly stale) index entry
    let files: Vec<&FileInfo> = std::iter::once(info)
        .chain(index.files.values().filter(|f| path_key(&f.path) != path_key(&info.path)))
        .collect();
    let mut out = vec![];
    match target {
        Target::Call { name, .. } | Target::FuncDef { name } => {
            for f in &files {
                for c in f.calls.iter().filter(|c| c.name == name) {
                    out.extend(loc(&f.path, c.range));
                }
                if include_decl {
                    for d in f.funcs.iter().filter(|d| d.name == name) {
                        out.extend(loc(&f.path, d.name_range));
                    }
                }
            }
        }
        Target::Id { id } => {
            for f in &files {
                for (i, r) in f.id_refs.iter().filter(|(i, _)| *i == id) {
                    let _ = i;
                    out.extend(loc(&f.path, *r));
                }
            }
        }
        Target::MapKey { name, node } => {
            // the same key in literals and m.key accesses of the function
            let mut stack = vec![enclosing_function(&node).unwrap_or(tree.root_node())];
            while let Some(n) = stack.pop() {
                if n.kind() == "identifier" && is_map_key(&n) && node_text(&n, src) == name {
                    out.extend(loc(&info.path, li.node_range(&n)));
                }
                stack.extend(super::index::named_children(&n));
            }
        }
        Target::Ident { name, node } => {
            // local variable: references within the function
            if let Some(func) = enclosing_function(&node) {
                if local_declaration(&func, &name, src, &li).is_some() {
                    let mut stack = vec![func];
                    while let Some(n) = stack.pop() {
                        if n.kind() == "identifier" && !is_map_key(&n) && node_text(&n, src) == name {
                            out.extend(loc(&info.path, li.node_range(&n)));
                        }
                        stack.extend(super::index::named_children(&n));
                    }
                    return out;
                }
            }
        }
    }
    out
}

/// Ranges of the keys `name` in the map literals below `scope`.
fn map_keys(scope: &Node, name: &str, src: &str, li: &LineIndex) -> Vec<Range> {
    let mut out = vec![];
    let mut stack = vec![*scope];
    while let Some(n) = stack.pop() {
        if n.kind() == "map_key" {
            if let Some(k) = n.named_child(0).filter(|k| node_text(k, src) == name) {
                out.push(li.node_range(&k));
            }
        }
        stack.extend(super::index::named_children(&n));
    }
    out.sort_by_key(|r| (r.start.line, r.start.character));
    out
}

fn enclosing_function<'t>(n: &Node<'t>) -> Option<Node<'t>> {
    let mut cur = n.parent();
    while let Some(c) = cur {
        if c.kind() == "function_definition" || c.kind() == "old_function_definition" {
            return Some(c);
        }
        cur = c.parent();
    }
    None
}

/// Range of the parameter or `var` declaring `name` in function `f`.
fn local_declaration(f: &Node, name: &str, src: &str, li: &LineIndex) -> Option<Range> {
    let mut stack = vec![*f];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "param" => {
                if let Some(x) = n.child_by_field_name("name") {
                    if node_text(&x, src) == name {
                        return Some(li.node_range(&x));
                    }
                }
            }
            "var_assignment" => {
                if let Some(x) = super::index::named_children(&n).first() {
                    if node_text(x, src) == name {
                        return Some(li.node_range(x));
                    }
                }
            }
            "for_statement" => {
                let kids = super::index::children(&n);
                for w in kids.windows(2) {
                    if w[0].kind() == "var" && w[1].kind() == "identifier" && node_text(&w[1], src) == name {
                        return Some(li.node_range(&w[1]));
                    }
                }
            }
            _ => {}
        }
        let mut kids = super::index::named_children(&n);
        kids.reverse();
        stack.extend(kids);
    }
    None
}

#[allow(deprecated)]
pub fn document_symbols(info: &FileInfo) -> Vec<DocumentSymbol> {
    let mut out = vec![];
    for f in &info.funcs {
        out.push(DocumentSymbol {
            name: f.name.clone(),
            detail: Some(f.signature()),
            kind: SymbolKind::FUNCTION,
            tags: None,
            deprecated: None,
            range: f.range,
            selection_range: f.name_range,
            children: None,
        });
    }
    for v in &info.vars {
        out.push(DocumentSymbol {
            name: v.name.clone(),
            detail: Some(format!("{:?}", v.kind)),
            kind: if v.kind == VarKind::StaticConst { SymbolKind::CONSTANT } else { SymbolKind::VARIABLE },
            tags: None,
            deprecated: None,
            range: v.range,
            selection_range: v.range,
            children: None,
        });
    }
    out.sort_by_key(|s| (s.range.start.line, s.range.start.character));
    out
}

#[allow(deprecated)]
pub fn workspace_symbols(index: &Index, query: &str) -> Vec<SymbolInformation> {
    let q = query.to_lowercase();
    let mut out = vec![];
    for f in index.files.values() {
        for d in &f.funcs {
            if d.name.to_lowercase().contains(&q) {
                if let Some(l) = loc(&f.path, d.name_range) {
                    out.push(SymbolInformation {
                        name: d.name.clone(),
                        kind: SymbolKind::FUNCTION,
                        tags: None,
                        deprecated: None,
                        location: l,
                        container_name: f.def_id.clone(),
                    });
                }
            }
        }
    }
    for i in index.ids.values().flatten() {
        let name = i.name.clone().unwrap_or_default();
        if i.id.to_lowercase().contains(&q) || name.to_lowercase().contains(&q) {
            let p = Position::new(i.line, 0);
            if let Some(l) = loc(&i.defcore, Range::new(p, p)) {
                out.push(SymbolInformation {
                    name: format!("{} ({})", i.id, name),
                    kind: SymbolKind::CLASS,
                    tags: None,
                    deprecated: None,
                    location: l,
                    container_name: None,
                });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.truncate(200);
    out
}
