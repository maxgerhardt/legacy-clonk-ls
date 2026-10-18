//! Autocomplete and hover for DefCore.txt, Scenario.txt and ActMap.txt.
//!
//! Sections, keys and value kinds come from the engine source (core/ini_api.json, tools/gen_ini_api.py): after `[`
//! the section names, at the start of a line the keys of the section not written yet, after `Key=` the values the
//! key takes: named flags (`C4D_Object|C4D_Select...`, CNAT_*), IDs and ID lists (`FLNT=3;`), procedures, the
//! actions of the ActMap (NextAction), functions of the definition's script (StartCall, TimerCall), materials and
//! Material-Texture pairs, packs (Definition1), 0/1 and the engine's default.

use super::index::{path_key, Index};
use super::text::LineIndex;
use crate::lsp::shared::definition::IniDefsProvider;
use lazy_static::lazy_static;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind, Documentation, MarkupContent, MarkupKind, Position};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IniFile {
    DefCore,
    ActMap,
    Scenario,
}

#[derive(Debug, Deserialize)]
pub struct IniKey {
    pub key: String,
    pub kind: String,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub values: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct IniApi {
    files: HashMap<String, HashMap<String, Vec<IniKey>>>,
    materials: Vec<String>,
    texmap: Vec<String>,
    packs: Vec<String>,
}

const RAW: &str = include_str!("../core/ini_api.json");

lazy_static! {
    static ref API: IniApi = serde_json::from_str(RAW).expect("ini_api.json is invalid");
}

impl IniFile {
    fn name(self) -> &'static str {
        match self {
            IniFile::DefCore => "DefCore",
            IniFile::ActMap => "ActMap",
            IniFile::Scenario => "Scenario",
        }
    }

    fn sections(self) -> Vec<&'static str> {
        let mut s: Vec<&str> = API.files[self.name()].keys().map(|k| k.as_str()).collect();
        // file order, not alphabetical
        let order = ["DefCore", "Physical", "Action", "Head", "Definitions", "Game", "Player1", "Player2", "Player3", "Player4", "Landscape", "Animals", "Weather", "Disasters", "Environment"];
        s.sort_by_key(|n| order.iter().position(|o| o == n).unwrap_or(99));
        s
    }

    fn keys(self, section: &str) -> &'static [IniKey] {
        API.files[self.name()].get(section).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Description of a key from the translated docs (lang/*.json via the handlers' defs.csv).
    fn description(self, section: &str, key: &str) -> Option<String> {
        let def = match self {
            IniFile::DefCore => crate::lsp::def_core_handler::DefCoreDefs::get_def(section, key),
            IniFile::ActMap => crate::lsp::act_map_handler::ActMapDefs::get_def(section, key),
            IniFile::Scenario => crate::lsp::scenario_txt_handler::definition::ScenarioTxtDefs::get_def(section, key),
        }?;
        crate::lang::Translation::get_translation(def.description).cloned()
    }
}

/// What a value of this kind looks like, for the item detail / hover.
fn kind_text(k: &IniKey) -> String {
    let what = match k.kind.as_str() {
        "flags" => "flags, combined with |",
        "id" => "definition ID",
        "idlist" => "ID list: ID=count;ID=count",
        "sval" => "Std,Random,Min,Max",
        "bool" => "0 or 1",
        "ints" => "comma-separated numbers",
        "procedure" => "procedure",
        "action" => "action name of this ActMap",
        "function" => "function of the definition's script",
        "material" => "material (Material-Texture)",
        "materials" => "materials: Name=amount;",
        "pack" | "packs" => "pack (.c4d/.c4f)",
        "string" => "text",
        _ => "number",
    };
    match &k.default {
        Some(d) if !d.is_empty() && d != "\"\"" => format!("{} (default {})", what, clean_default(d)),
        _ => what.to_string(),
    }
}

/// C4SVal(100, 0, 64, 250) -> 100,0,64,250; "Earth" -> Earth
fn clean_default(d: &str) -> String {
    let d = d.trim();
    if let Some(inner) = d.strip_prefix("C4SVal(").and_then(|x| x.strip_suffix(')')) {
        return inner.split(',').map(|p| p.trim()).collect::<Vec<_>>().join(",");
    }
    d.trim_matches('"').to_string()
}

fn doc(md: String) -> Option<Documentation> {
    (!md.is_empty()).then(|| Documentation::MarkupContent(MarkupContent { kind: MarkupKind::Markdown, value: md }))
}

struct Lines<'a> {
    lines: Vec<&'a str>,
}

impl<'a> Lines<'a> {
    /// The section the line `row` is in and the range of rows of that section.
    fn section_at(&self, row: usize) -> Option<(&'a str, usize, usize)> {
        let start = (0..=row.min(self.lines.len().saturating_sub(1))).rev().find(|&r| self.section_name(r).is_some())?;
        let end = (start + 1..self.lines.len()).find(|&r| self.section_name(r).is_some()).unwrap_or(self.lines.len());
        Some((self.section_name(start)?, start, end))
    }

    fn section_name(&self, row: usize) -> Option<&'a str> {
        let l = self.lines.get(row)?.trim();
        l.strip_prefix('[')?.split(']').next().map(|s| s.trim())
    }

    fn key_of(&self, row: usize) -> Option<&'a str> {
        let l = self.lines.get(row)?;
        let (k, _) = l.split_once('=')?;
        Some(k.trim())
    }
}

pub struct Ctx<'a> {
    pub index: &'a Index,
    pub path: &'a Path,
    pub game_dir: Option<&'a Path>,
}

/// Completion items at `pos` in an INI file.
pub fn complete(file: IniFile, src: &str, pos: Position, ctx: &Ctx) -> Option<Vec<CompletionItem>> {
    let li = LineIndex::new(src);
    let at = li.to_byte(pos).min(src.len());
    let line_start = src[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let before = &src[line_start..at];
    let after = src[at..].split('\n').next().unwrap_or("");
    let lines = Lines { lines: src.split('\n').collect() };
    let row = pos.line as usize;
    let mut out = vec![];
    let mut seen = HashSet::new();
    let mut add = |label: &str, kind: CompletionItemKind, detail: String, documentation: Option<Documentation>, insert: Option<String>, sort: usize| {
        if seen.insert(label.to_string()) {
            out.push(CompletionItem {
                label: label.to_string(),
                kind: Some(kind),
                detail: (!detail.is_empty()).then_some(detail),
                documentation,
                insert_text: insert,
                sort_text: Some(format!("{:04}", sort)),
                ..Default::default()
            });
        }
    };
    let trimmed = before.trim_start();
    if trimmed.starts_with(';') || trimmed.starts_with('#') || trimmed.starts_with("//") {
        return None; // comment line
    }

    // [Section]
    if let Some(partial) = trimmed.strip_prefix('[') {
        if partial.contains(']') {
            return None;
        }
        let close = !after.trim_start().starts_with(']');
        for (i, s) in file.sections().into_iter().enumerate() {
            add(s, CompletionItemKind::MODULE, format!("[{}]", s), None, close.then(|| format!("{}]", s)), i);
        }
        return Some(out);
    }

    let section = lines.section_at(row);
    // Key=
    let Some(eq) = before.find('=') else {
        let Some((name, start, end)) = section else {
            // before any section: the sections
            for (i, s) in file.sections().into_iter().enumerate() {
                add(&format!("[{}]", s), CompletionItemKind::MODULE, String::new(), None, Some(format!("[{}]", s)), i);
            }
            return Some(out);
        };
        let present: HashSet<&str> = (start + 1..end).filter(|&r| r != row).filter_map(|r| lines.key_of(r)).collect();
        for (i, k) in file.keys(name).iter().enumerate() {
            if present.contains(k.key.as_str()) {
                continue;
            }
            let mut md = file.description(name, &k.key).unwrap_or_default();
            if !k.values.is_empty() {
                md.push_str(&format!("\n\nValues: {}", k.values.iter().map(|v| format!("`{}`", v)).collect::<Vec<_>>().join(", ")));
            }
            let insert = if after.trim_start().starts_with('=') { k.key.clone() } else { format!("{}=", k.key) };
            add(&k.key, CompletionItemKind::PROPERTY, kind_text(k), doc(md.trim().to_string()), Some(insert), i);
        }
        return Some(out);
    };

    // Key=value: the element being typed (after the last | for flags, ; for lists)
    let (sec_name, _, _) = section?;
    let key = before[..eq].trim();
    let spec = file.keys(sec_name).iter().find(|k| k.key == key)?;
    let value = &before[eq + 1..];
    let element = value.rsplit(|c| c == '|' || c == ';' || c == ',').next().unwrap_or("").trim_start();
    if matches!(spec.kind.as_str(), "idlist" | "materials") && element.contains('=') {
        return None; // the count of ID=count
    }
    let list_insert = |name: &str| -> Option<String> {
        matches!(spec.kind.as_str(), "idlist").then(|| format!("{}=1", name))
    };
    match spec.kind.as_str() {
        "flags" | "procedure" => {
            for (i, v) in spec.values.iter().enumerate() {
                add(v, CompletionItemKind::ENUM_MEMBER, String::new(), None, None, i);
            }
        }
        "bool" => {
            add("0", CompletionItemKind::VALUE, "false".to_string(), None, None, 0);
            add("1", CompletionItemKind::VALUE, "true".to_string(), None, None, 1);
        }
        "id" | "idlist" => {
            let mut ids: Vec<(String, String)> = ctx.index.ids.keys()
                .filter_map(|id| ctx.index.id_for(id, ctx.path).map(|i| (id.clone(), i.name.clone().unwrap_or_default())))
                .collect();
            for i in crate::workspace::Workspace::new().get_ids() {
                ids.push((i.id.clone(), format!("{} ({})", i.name, i.tlc)));
            }
            ids.sort();
            for (n, (id, name)) in ids.iter().enumerate() {
                add(id, CompletionItemKind::CLASS, name.clone(), None, list_insert(id), n);
            }
        }
        "action" => {
            let names: Vec<&str> = (0..lines.lines.len())
                .filter(|&r| lines.key_of(r) == Some("Name"))
                .filter_map(|r| lines.lines[r].split_once('=').map(|(_, v)| v.trim()))
                .filter(|v| !v.is_empty())
                .collect();
            for (i, n) in names.iter().enumerate() {
                add(n, CompletionItemKind::ENUM_MEMBER, "action of this ActMap".to_string(), None, None, i);
            }
            add("Hold", CompletionItemKind::KEYWORD, "keep the last phase".to_string(), None, None, 900);
            add("Idle", CompletionItemKind::KEYWORD, "no action afterwards".to_string(), None, None, 901);
        }
        "function" => {
            // the definition's Script.c (and what it #includes / #appendto)
            let script = ctx.path.parent().map(|d| d.join("Script.c"));
            if let Some(f) = script.as_ref().and_then(|s| ctx.index.files.get(&path_key(s))) {
                for (i, s) in ctx.index.scope_files(f).into_iter().enumerate() {
                    for d in &s.funcs {
                        add(&d.name, CompletionItemKind::FUNCTION, format!("{}  ({})", d.signature(), s.def_id.as_deref().unwrap_or("script")), None, None, i);
                    }
                }
            }
        }
        "material" | "materials" => {
            // the scenario's own Material.c4g (unpacked) first
            let mut own = vec![];
            if let Some(dir) = ctx.path.parent().map(|d| d.join("Material.c4g")).filter(|d| d.is_dir()) {
                for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                    let n = e.file_name().to_string_lossy().to_string();
                    if let Some(stem) = n.strip_suffix(".c4m") {
                        own.push(stem.to_string());
                    }
                }
            }
            for (i, m) in own.iter().enumerate() {
                add(m, CompletionItemKind::COLOR, "material of this scenario".to_string(), None, None, i);
            }
            for (i, t) in API.texmap.iter().enumerate() {
                add(t, CompletionItemKind::COLOR, "Material-Texture".to_string(), None, None, 100 + i);
            }
            for (i, m) in API.materials.iter().enumerate() {
                add(m, CompletionItemKind::COLOR, "material".to_string(), None, None, 200 + i);
            }
        }
        "pack" | "packs" => {
            let mut packs: Vec<String> = vec![];
            if let Some(g) = ctx.game_dir {
                for e in std::fs::read_dir(g).into_iter().flatten().flatten() {
                    let n = e.file_name().to_string_lossy().to_string();
                    let l = n.to_lowercase();
                    if l.ends_with(".c4d") || l.ends_with(".c4f") {
                        packs.push(n);
                    }
                }
            }
            packs.extend(API.packs.iter().cloned());
            packs.sort();
            for (i, p) in packs.iter().enumerate() {
                add(p, CompletionItemKind::FOLDER, String::new(), None, None, i);
            }
        }
        _ => {}
    }
    // the engine's default is always a fine value
    if let Some(d) = spec.default.as_deref().map(clean_default).filter(|d| !d.is_empty() && element.is_empty() && spec.kind != "idlist") {
        if d.chars().all(|c| c.is_ascii_digit() || c == ',' || c == '-') {
            add(&d, CompletionItemKind::VALUE, "engine default".to_string(), None, None, 999);
        }
    }
    Some(out)
}

/// Hover on a key: its description, kind, default and values; on an ID: the definition.
pub fn hover(file: IniFile, src: &str, pos: Position, ctx: &Ctx) -> Option<String> {
    let lines = Lines { lines: src.split('\n').collect() };
    let row = pos.line as usize;
    let line = lines.lines.get(row)?;
    let col = LineIndex::new(line).to_byte(Position::new(0, pos.character)).min(line.len());
    let (sec, _, _) = lines.section_at(row)?;
    let eq = line.find('=');
    if eq.map_or(true, |e| col <= e) {
        let key = line[..eq.unwrap_or(line.len())].trim();
        let spec = file.keys(sec).iter().find(|k| k.key == key)?;
        let mut s = format!("**{}** - {}", key, kind_text(spec));
        if let Some(d) = file.description(sec, key) {
            s.push_str(&format!("\n\n{}", d));
        }
        if !spec.values.is_empty() {
            s.push_str(&format!("\n\nValues: {}", spec.values.iter().map(|v| format!("`{}`", v)).collect::<Vec<_>>().join(", ")));
        }
        return Some(s);
    }
    // the word under the cursor in the value: an ID?
    let b = line.as_bytes();
    let is_w = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut s0 = col;
    while s0 > 0 && is_w(b[s0 - 1]) {
        s0 -= 1;
    }
    let mut e0 = col;
    while e0 < b.len() && is_w(b[e0]) {
        e0 += 1;
    }
    let word = &line[s0..e0];
    if word.len() == 4 {
        if let Some(i) = ctx.index.id_for(word, ctx.path).or_else(|| ctx.index.id_any(word)) {
            return Some(format!("**{}** - {}\n\n`{}`", word, i.name.clone().unwrap_or_default(), i.defcore.display()));
        }
        if let Some(i) = super::diagnostics::standard_id(word) {
            return Some(format!("**{}** - {}\n\nstandard definition from `{}`", word, i.name, i.tlc));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(file: IniFile, src: &str) -> Vec<String> {
        let index = Index::build(vec![]);
        let ctx = Ctx { index: &index, path: Path::new("X.c4d/DefCore.txt"), game_dir: None };
        let lines: Vec<&str> = src.split('\n').collect();
        let pos = Position::new(lines.len() as u32 - 1, lines.last().unwrap().len() as u32);
        complete(file, src, pos, &ctx).unwrap_or_default().into_iter().map(|i| i.label).collect()
    }

    #[test]
    fn sections_keys_and_values() {
        assert!(labels(IniFile::DefCore, "[").contains(&"Physical".to_string()));
        let keys = labels(IniFile::DefCore, "[DefCore]\nid=ABCD\nName=x\n");
        assert!(keys.contains(&"Category".to_string()) && keys.contains(&"Width".to_string()));
        assert!(!keys.contains(&"id".to_string()), "a key already written is not offered again");
        let cats = labels(IniFile::DefCore, "[DefCore]\nCategory=C4D_Object|");
        assert!(cats.contains(&"C4D_SelectMaterial".to_string()));
        let procs = labels(IniFile::ActMap, "[Action]\nName=Walk\nProcedure=");
        assert!(procs.contains(&"WALK".to_string()) && procs.contains(&"NONE".to_string()));
        let next = labels(IniFile::ActMap, "[Action]\nName=Walk\n\n[Action]\nName=Jump\nNextAction=");
        assert!(next.contains(&"Walk".to_string()) && next.contains(&"Jump".to_string()) && next.contains(&"Hold".to_string()));
        let attach = labels(IniFile::ActMap, "[Action]\nAttach=");
        assert!(attach.contains(&"CNAT_Bottom".to_string()));
        let scen = labels(IniFile::Scenario, "[Landscape]\nLiquid=");
        assert!(scen.contains(&"Water-Smooth".to_string()) || scen.contains(&"Water".to_string()));
        let gravity = labels(IniFile::Scenario, "[Landscape]\nGravity=");
        assert_eq!(gravity, vec!["100,0,10,200".to_string()]);
        assert!(labels(IniFile::Scenario, "[Definitions]\nDefinition1=").contains(&"Objects.c4d".to_string()));
        assert!(labels(IniFile::Scenario, "[Player1]\nCrew=CLNK=").is_empty(), "no IDs for the count");
    }
}
