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
        Some(d) if d.is_empty() => format!("{} (empty by default)", what),
        Some(d) => format!("{} (default {})", what, d),
        None => what.to_string(),
    }
}

/// The value part of a default ("0 (1 for GOLD)" -> 0; "all" and "the value of X": none)
fn default_value(d: &str) -> Option<String> {
    let v = d.split(" (").next().unwrap_or("").trim();
    (!v.is_empty() && v != "all" && !v.starts_with("the value of")).then(|| v.to_string())
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
    if let Some(d) = spec.default.as_deref().and_then(default_value).filter(|_| element.is_empty() && spec.kind != "idlist") {
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

/// Width and height of a PNG or BMP image (from its header).
pub fn image_size(path: &Path) -> Option<(u32, u32)> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = [0u8; 26];
    std::io::Read::read_exact(&mut f, &mut h).ok()?;
    if &h[1..4] == b"PNG" {
        let w = u32::from_be_bytes([h[16], h[17], h[18], h[19]]);
        let hh = u32::from_be_bytes([h[20], h[21], h[22], h[23]]);
        return Some((w, hh));
    }
    if &h[0..2] == b"BM" {
        let w = i32::from_le_bytes([h[18], h[19], h[20], h[21]]).unsigned_abs();
        let hh = i32::from_le_bytes([h[22], h[23], h[24], h[25]]).unsigned_abs();
        return Some((w, hh));
    }
    None
}

/// The definition's base graphics (Graphics.png, else Graphics.bmp: C4DefGraphics::LoadGraphics).
fn graphics_file(dir: &Path) -> Option<std::path::PathBuf> {
    ["Graphics.png", "Graphics.bmp"].iter().map(|n| dir.join(n)).find(|p| p.is_file())
}

/// Problems of a DefCore.txt / ActMap.txt / Scenario.txt the engine passes over silently (or with a load error):
/// unknown sections/keys (StdCompilerINIRead matches names case-sensitively and ignores the rest), values of the
/// wrong kind (flags, IDs, numbers, 0/1, procedures, actions, functions), a definition without Graphics.png/.bmp
/// (the engine doesn't load it), Picture/Facet rectangles outside the image, an id= used by another definition.
pub fn check(file: IniFile, src: &str, ctx: &Ctx) -> Vec<tower_lsp::lsp_types::Diagnostic> {
    use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Range};
    let mut out: Vec<Diagnostic> = vec![];
    let lines: Vec<&str> = src.lines().collect();
    let mut push = |row: usize, start: usize, end: usize, sev: DiagnosticSeverity, code: &str, msg: String| {
        let line = lines.get(row).copied().unwrap_or("");
        let (a, b) = (line[..start.min(line.len())].encode_utf16().count() as u32, line[..end.min(line.len())].encode_utf16().count() as u32);
        out.push(Diagnostic {
            range: Range::new(Position::new(row as u32, a), Position::new(row as u32, b.max(a + 1))),
            severity: Some(sev),
            code: Some(NumberOrString::String(code.to_string())),
            source: Some("c4script".to_string()),
            message: msg,
            ..Default::default()
        });
    };
    let dir = ctx.path.parent().unwrap_or(Path::new("."));
    let sections = file.sections();
    let is_section = |n: &str| sections.contains(&n) || (file == IniFile::Scenario && n.starts_with("Player") && n[6..].parse::<u32>().map(|i| (1..=4).contains(&i)).unwrap_or(false));
    // actions of this ActMap (NextAction etc.) and the definition's script (StartCall etc.)
    let actmap_names: Vec<String> = if file == IniFile::ActMap {
        lines.iter().filter_map(|l| l.trim().strip_prefix("Name=").map(|n| n.trim().to_string())).collect()
    } else {
        super::assets::actions(dir).unwrap_or_default()
    };
    let script = ctx.index.files.get(&path_key(&dir.join("Script.c")));
    let has_func = |f: &str| match script {
        Some(info) => !ctx.index.visible_funcs(info, f).is_empty(),
        None => true, // no indexed script: can't tell
    };
    let known_id = |id: &str| id == "NONE" || ctx.index.id_for(id, ctx.path).is_some() || ctx.index.id_any(id).is_some() || super::diagnostics::standard_id(id).is_some();
    // the image in DefCore units: C4Facet::DrawX multiplies source rectangles by Scale (percent, [DefCore] Scale=);
    // a tiny placeholder (graphics set at runtime from another definition) isn't checked
    let scale: f64 = std::fs::read(dir.join("DefCore.txt")).ok().map(|b| super::text::decode_bytes(&b)).and_then(|t| {
        let mut sec = "";
        for l in t.lines() {
            let l = l.trim();
            if l.starts_with('[') { sec = l; continue; }
            if sec == "[DefCore]" {
                if let Some(v) = l.strip_prefix("Scale=") { return v.trim().parse::<f64>().ok(); }
            }
        }
        None
    }).unwrap_or(100.0) / 100.0;
    // a script that swaps graphics at runtime (SetGraphics, often from another definition) isn't judged by its sheet
    let swaps = std::fs::read(dir.join("Script.c")).map(|b| String::from_utf8_lossy(&b).contains("SetGraphics(")).unwrap_or(false);
    let image = graphics_file(dir).and_then(|g| image_size(&g)).filter(|(w, h)| !swaps && *w > 8 && *h > 8)
        .map(|(w, h)| (((w as f64) / scale).round() as u32, ((h as f64) / scale).round() as u32));
    let is_int = |v: &str| { let v = v.trim(); !v.is_empty() && (v.parse::<i64>().is_ok() || v.strip_prefix("0x").map(|h| i64::from_str_radix(h, 16).is_ok()).unwrap_or(false)) };
    let ints = |v: &str| -> Option<Vec<i64>> { v.trim_end_matches(';').split(',').map(|x| x.trim().parse::<i64>().ok()).collect() };
    let mut section = String::new();
    // per [Action]: name row, facet, length, directions, facet base
    struct Act { row: usize, name: Option<String>, facet: Option<(usize, Vec<i64>)>, length: i64, dirs: i64, flip: i64, base: bool }
    let mut acts: Vec<Act> = vec![];
    let mut defcore_id: Option<(usize, usize, String)> = None;
    let mut size: (Option<i64>, Option<i64>) = (None, None);
    let mut picture: Option<(usize, Vec<i64>)> = None;
    for (row, raw) in lines.iter().enumerate() {
        let t = raw.trim();
        if t.is_empty() || t.starts_with(';') || t.starts_with('#') || t.starts_with("//") {
            continue;
        }
        let indent = raw.len() - raw.trim_start().len();
        if let Some(name) = t.strip_prefix('[').and_then(|x| x.strip_suffix(']')) {
            section = name.to_string();
            if !is_section(name) {
                push(row, indent, indent + t.len(), DiagnosticSeverity::WARNING, "ini-section",
                    format!("unknown section [{}] (the engine ignores it; sections: {})", name, sections.join(", ")));
            }
            if file == IniFile::ActMap && name == "Action" {
                acts.push(Act { row, name: None, facet: None, length: 1, dirs: 1, flip: 0, base: false });
            }
            continue;
        }
        let Some((k, v)) = t.split_once('=') else { continue };
        let (key, value) = (k.trim(), v.trim());
        if !is_section(&section) {
            continue;
        }
        let sec_keys = file.keys(if section.starts_with("Player") { "Player1" } else { &section });
        let Some(spec) = sec_keys.iter().find(|x| x.key == key) else {
            let hint = sec_keys.iter().find(|x| x.key.eq_ignore_ascii_case(key)).map(|x| format!(" (names are case-sensitive: {})", x.key)).unwrap_or_default();
            push(row, indent, indent + key.len(), DiagnosticSeverity::WARNING, "ini-key",
                format!("unknown key {} in [{}]: the engine ignores it{}", key, section, hint));
            continue;
        };
        let vstart = indent + t.find('=').unwrap_or(0) + 1;
        let vstart = vstart + (raw.len() - vstart).saturating_sub(raw[vstart.min(raw.len())..].trim_start().len());
        let vend = vstart + value.len();
        // unknown IDs in a Scenario.txt usually come from packs that aren't indexed: only a hint there
        let id_sev = if file == IniFile::Scenario { DiagnosticSeverity::INFORMATION } else { DiagnosticSeverity::WARNING };
        let none = value.eq_ignore_ascii_case("none");
        let mut bad = |id: bool, msg: String| push(row, vstart, vend, if id { id_sev } else { DiagnosticSeverity::WARNING }, "ini-value", msg);
        match spec.kind.as_str() {
            "flags" if !value.is_empty() => {
                for f in value.split('|').map(|x| x.trim()) {
                    if !f.is_empty() && !is_int(f) && !spec.values.iter().any(|x| x == f) {
                        bad(false, format!("{}: unknown flag {} (the engine reads it as 0)", key, f));
                    }
                }
            }
            "id" if !value.is_empty() && !known_id(value) => bad(true, format!("{}: unknown definition ID {}", key, value)),
            "idlist" if !value.is_empty() => {
                for entry in value.split(';').map(|x| x.trim()).filter(|x| !x.is_empty()) {
                    let (id, count) = entry.split_once('=').map(|(a, b)| (a.trim(), Some(b.trim()))).unwrap_or((entry, None));
                    if !known_id(id) {
                        bad(true, format!("{}: unknown definition ID {}", key, id));
                    } else if count.map(|c| !is_int(c)).unwrap_or(false) {
                        bad(false, format!("{}: the count of {} isn't a number", key, id));
                    }
                }
            }
            "int" if !is_int(value) && !value.is_empty() && ints(value).is_none() => bad(false, format!("{}: {} isn't a number", key, value)),
            "ints" | "sval" if !value.is_empty() && ints(value).is_none() => bad(false, format!("{}: expected numbers separated by commas", key)),
            "bool" if !matches!(value, "0" | "1" | "") => bad(false, format!("{}: expected 0 or 1", key)),
            "procedure" if !value.is_empty() && !none && !spec.values.iter().any(|x| x == value) => bad(false, format!("{}: unknown procedure {} (the engine treats it as NONE)", key, value)),
            "action" if !value.is_empty() && !none && !matches!(value, "Hold" | "Idle") && !actmap_names.iter().any(|a| a == value) =>
                bad(false, format!("{}: no action {} in this ActMap (only Hold and Idle besides)", key, value)),
            "function" if !value.is_empty() && !none && !has_func(value) => bad(false, format!("{}: the definition's script has no function {} (the engine calls nothing)", key, value)),
            _ => {}
        }
        // remembered for the cross checks below
        match (file, section.as_str(), key) {
            (IniFile::DefCore, "DefCore", "id") => defcore_id = Some((row, vstart, value.to_string())),
            (IniFile::DefCore, "DefCore", "Width") => size.0 = value.parse().ok(),
            (IniFile::DefCore, "DefCore", "Height") => size.1 = value.parse().ok(),
            (IniFile::DefCore, "DefCore", "Picture") => picture = ints(value).map(|v| (row, v)),
            (IniFile::ActMap, "Action", _) => {
                if let Some(a) = acts.last_mut() {
                    match key {
                        "Name" => a.name = Some(value.to_string()),
                        "Facet" => a.facet = ints(value).map(|v| (row, v)),
                        "Length" => a.length = value.parse().unwrap_or(1).max(1),
                        "Directions" => a.dirs = value.parse().unwrap_or(1).max(1),
                        "FacetBase" => a.base = value == "1",
                        "FlipDir" => a.flip = value.parse().unwrap_or(0),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    // DefCore: graphics, picture, size, id collisions
    if file == IniFile::DefCore {
        if graphics_file(dir).is_none() {
            push(0, 0, lines.first().map(|l| l.len()).unwrap_or(1), DiagnosticSeverity::ERROR, "def-graphics",
                "no Graphics.png or Graphics.bmp in this definition: the engine doesn't load it (\"Error loading graphics\")".to_string());
        }
        if let Some((w, h)) = image {
            if let (Some(sw), Some(sh)) = size {
                if acts.is_empty() && super::assets::actions(dir).is_none() && (sw > w as i64 || sh > h as i64) {
                    if let Some(r) = lines.iter().position(|l| l.trim_start().starts_with("Width=")) {
                        push(r, 0, lines[r].len(), DiagnosticSeverity::WARNING, "ini-value",
                            format!("Width/Height {}x{} is bigger than the graphics ({}x{}): the sprite is cut", sw, sh, w, h));
                    }
                }
            }
            if let Some((row, p)) = &picture {
                if p.len() >= 4 && (p[0] + p[2] > w as i64 || p[1] + p[3] > h as i64 || p[0] < 0 || p[1] < 0) {
                    push(*row, 0, lines[*row].len(), DiagnosticSeverity::WARNING, "ini-value",
                        format!("Picture {},{},{},{} lies outside the graphics ({}x{})", p[0], p[1], p[2], p[3], w, h));
                }
            }
        }
        if let Some((row, col, id)) = &defcore_id {
            // what loads together: a scenario's own definitions (up to its .c4s), else the outermost pack/folder
            let outer = |p: &Path| -> String {
                let k = path_key(p);
                if let Some(i) = k.find(".c4s/") {
                    return k[..i + 5].to_string();
                }
                let mut acc = String::new();
                for seg in k.split('/') {
                    acc.push_str(seg);
                    acc.push('/');
                    if seg.ends_with(".c4d") || seg.ends_with(".c4f") {
                        return acc;
                    }
                }
                acc
            };
            let mine = outer(ctx.path);
            let others: Vec<std::path::PathBuf> = ctx.index.ids.get(id.as_str()).into_iter().flatten()
                .map(|i| i.defcore.clone())
                .filter(|p| path_key(p) != path_key(ctx.path) && outer(p) == mine)
                // an identical copy (reference/ next to content/) isn't a second definition
                .filter(|p| std::fs::read(p).ok() != Some(src.as_bytes().to_vec()) && std::fs::read(p).ok() != std::fs::read(ctx.path).ok())
                .collect();
            if let Some(o) = others.first() {
                push(*row, *col, *col + id.len(), DiagnosticSeverity::WARNING, "ini-id",
                    format!("id {} is also used by {}{} (where both load, the more local one replaces the other)",
                        id, o.parent().unwrap_or(o).display(), if others.len() > 1 { format!(" and {} more", others.len() - 1) } else { String::new() }));
            }
        }
    }
    // ActMap: duplicate names, facets inside the graphics
    if file == IniFile::ActMap {
        let mut seen: HashSet<String> = HashSet::new();
        for a in &acts {
            if let Some(n) = &a.name {
                if !seen.insert(n.clone()) {
                    push(a.row, 0, 8, DiagnosticSeverity::WARNING, "ini-value", format!("a second action {}: SetAction finds the first one", n));
                }
            }
            if let (Some((row, f)), Some((w, h)), false) = (&a.facet, image, a.base) {
                if f.len() >= 4 && f[2] > 0 && f[3] > 0 {
                    // directions from FlipDir on are drawn mirrored from the lower rows (C4Object::UpdateFlipDir)
                    let rows = if a.flip > 0 { a.flip.min(a.dirs) } else { a.dirs };
                    let (right, bottom) = (f[0] + f[2] * a.length, f[1] + f[3] * rows);
                    if right > w as i64 || bottom > h as i64 || f[0] < 0 || f[1] < 0 {
                        push(*row, 0, lines[*row].len(), DiagnosticSeverity::WARNING, "ini-value",
                            format!("action {}: {} frames x {} directions of {}x{} from {},{} reach {},{} outside the graphics ({}x{})",
                                a.name.as_deref().unwrap_or("?"), a.length, a.dirs, f[2], f[3], f[0], f[1], right, bottom, w, h));
                    }
                }
            }
        }
    }
    out
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
