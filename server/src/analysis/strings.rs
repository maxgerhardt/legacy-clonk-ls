//! String tables: `$Name$` in a script is replaced by the entry `Name=...` of the StringTbl file next to it
//! (C4ScriptHost::Load -> C4LangStringTable::ReplaceStrings). Like the engine:
//! - the table of the script's own folder (definition, scenario or System.c4g): `StringTbl.txt` or
//!   `StringTbl<Language>.txt` (one per language, e.g. StringTblDE.txt / StringTblUS.txt)
//! - every `$Name$` in the whole script text counts (strings, function descriptions `[$Desc$]`, comments too),
//!   when Name consists of identifier characters (A-Z a-z 0-9 _) and is at most 30 characters long
//! - a missing entry logs a warning `string table entry not found: "Name"`
//! - table lines are `Name=Text` (everything after the first `=`)

use super::text::decode_bytes;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

lazy_static::lazy_static! {
    /// languages to check (setting stringTableLanguages, e.g. DE, US, ES); empty setting = DE, US
    static ref LANGUAGES: RwLock<Vec<String>> = RwLock::new(Vec::new());
}

pub fn set_languages(langs: Vec<String>) {
    if let Ok(mut l) = LANGUAGES.write() {
        *l = langs.into_iter().map(|x| x.trim().to_uppercase()).filter(|x| !x.is_empty()).collect();
    }
}

pub fn languages() -> Vec<String> {
    let l = LANGUAGES.read().map(|l| l.clone()).unwrap_or_default();
    if l.is_empty() { vec!["DE".to_string(), "US".to_string()] } else { l }
}

/// C4MaxName
const MAX_NAME: usize = 30;

pub struct StringTable {
    /// "" for StringTbl.txt, else the language code (DE, US, ...)
    pub lang: String,
    pub path: PathBuf,
    /// name -> (text, 0-based line)
    pub entries: HashMap<String, (String, u32)>,
}

/// The string tables of the folder `script` lies in, sorted by language.
pub fn tables_for(script: &Path) -> Vec<StringTable> {
    let Some(dir) = script.parent() else { return vec![] };
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut out = vec![];
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let lower = name.to_lowercase();
        if !(lower.starts_with("stringtbl") && lower.ends_with(".txt")) {
            continue;
        }
        let lang = name[9..name.len() - 4].to_string();
        let src = decode_bytes(&std::fs::read(e.path()).unwrap_or_default());
        let mut entries = HashMap::new();
        for (i, line) in src.lines().enumerate() {
            if let Some((k, v)) = line.split_once('=') {
                entries.entry(k.to_string()).or_insert((v.trim_end_matches('\r').to_string(), i as u32));
            }
        }
        out.push(StringTable { lang, path: e.path(), entries });
    }
    let langs = languages();
    // only the configured languages (and a language-neutral StringTbl.txt), in their order
    out.retain(|t| t.lang.is_empty() || langs.contains(&t.lang.to_uppercase()));
    out.sort_by_key(|t| langs.iter().position(|l| *l == t.lang.to_uppercase()).unwrap_or(usize::MAX));
    out
}

/// All `$Name$` references of a script as (name, start byte, end byte) - the '$' included - found the way the
/// engine scans the text.
pub fn refs(src: &str) -> Vec<(String, usize, usize)> {
    let b = src.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while let Some(off) = b[i..].iter().position(|&c| c == b'$') {
        let start = i + off;
        let mut j = start + 1;
        while j < b.len() && b[j] != b'$' && j - start - 1 < MAX_NAME {
            j += 1;
        }
        let closed = j < b.len() && b[j] == b'$';
        i = (j + 1).min(b.len());
        if !closed {
            continue;
        }
        let name = &b[start + 1..j];
        if !name.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_') {
            continue;
        }
        out.push((String::from_utf8_lossy(name).to_string(), start, j + 1));
        if i >= b.len() {
            break;
        }
    }
    out
}

/// The reference at byte offset `at`, if any.
pub fn ref_at(src: &str, at: usize) -> Option<(String, usize, usize)> {
    refs(src).into_iter().find(|(_, s, e)| (*s..*e).contains(&at))
}

/// Markdown for hover: the entry in every language.
pub fn hover(script: &Path, name: &str) -> String {
    let tables = tables_for(script);
    if tables.is_empty() {
        return format!("**${}$** - no StringTbl*.txt next to this script: the engine warns \"string table entry not found\"", name);
    }
    let mut s = format!("**${}$** *(string table)*", name);
    for l in languages() {
        if !tables.iter().any(|t| t.lang.to_uppercase() == l) {
            s.push_str(&format!("\n- **{}**: *no StringTbl{}.txt here (the engine falls back to the next language)*", l, l));
        }
    }
    for t in &tables {
        let lang = if t.lang.is_empty() { "all languages".to_string() } else { t.lang.clone() };
        match t.entries.get(name) {
            Some((text, line)) => s.push_str(&format!("\n- **{}**: {}  \n  `{}:{}`", lang, text, t.path.file_name().unwrap_or_default().to_string_lossy(), line + 1)),
            None => s.push_str(&format!("\n- **{}**: *missing in {}*", lang, t.path.file_name().unwrap_or_default().to_string_lossy())),
        }
    }
    s
}
