//! Names in strings that point at assets: actions (SetAction, GetAction comparisons), sounds (Sound,
//! SoundLevel) and graphics (SetGraphics). A wrong name does nothing in the engine, silently:
//! - C4Object::SetActionByName: the definition's own ActMap (or "Idle"/"ActIdle"), case-sensitive; else false
//! - C4SoundSystem: no extension -> ".wav" appended (PrepareFilename), '*' -> '?' = exactly one character
//!   (WildcardMatch), case-insensitive; sounds come from Sound.c4g groups and definitions; else nothing plays
//! - C4DefGraphics::Get: Graphics<Name>.png/.bmp of the definition (case-insensitive); else SetGraphics is false

use super::index::{FileInfo, Index};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

lazy_static::lazy_static! {
    static ref ACTMAPS: Mutex<HashMap<PathBuf, Option<Vec<String>>>> = Mutex::new(HashMap::new());
    static ref GRAPHICS: Mutex<HashMap<PathBuf, Vec<String>>> = Mutex::new(HashMap::new());
    static ref GAME_SOUNDS: Mutex<HashMap<PathBuf, Vec<String>>> = Mutex::new(HashMap::new());
}

/// Forget cached ActMaps/graphics (files may have changed).
pub fn clear_cache() {
    if let Ok(mut m) = ACTMAPS.lock() {
        m.clear();
    }
    if let Ok(mut m) = GRAPHICS.lock() {
        m.clear();
    }
}

/// The action names of the ActMap.txt in a definition folder (None: it has none).
pub fn actions(dir: &Path) -> Option<Vec<String>> {
    if let Some(r) = ACTMAPS.lock().ok().and_then(|m| m.get(dir).cloned()) {
        return r;
    }
    let r = std::fs::read(dir.join("ActMap.txt")).ok().map(|b| {
        super::text::decode_bytes(&b).lines()
            .filter_map(|l| l.trim().strip_prefix("Name=").map(|n| n.trim().to_string()))
            .collect::<Vec<_>>()
    });
    if let Ok(mut m) = ACTMAPS.lock() {
        m.insert(dir.to_path_buf(), r.clone());
    }
    r
}

/// Additional graphics names of a definition folder (Graphics<Name>.png / .bmp), lowercase.
pub fn graphics(dir: &Path) -> Vec<String> {
    if let Some(r) = GRAPHICS.lock().ok().and_then(|m| m.get(dir).cloned()) {
        return r;
    }
    let mut r = vec![];
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let n = e.file_name().to_string_lossy().to_lowercase();
        if let Some(rest) = n.strip_prefix("graphics") {
            if let Some(name) = rest.strip_suffix(".png").or_else(|| rest.strip_suffix(".bmp")) {
                if !name.is_empty() {
                    r.push(name.to_string());
                }
            }
        }
    }
    if let Ok(mut m) = GRAPHICS.lock() {
        m.insert(dir.to_path_buf(), r.clone());
    }
    r
}

/// The sound files of the game folder's packed Sound.c4g (c4group -l, cached), lowercase.
pub fn game_sounds(game_dir: &Path) -> Vec<String> {
    if let Some(r) = GAME_SOUNDS.lock().ok().and_then(|m| m.get(game_dir).cloned()) {
        return r;
    }
    let group = game_dir.join("Sound.c4g");
    let mut r = vec![];
    if group.is_dir() {
        for e in std::fs::read_dir(&group).into_iter().flatten().flatten() {
            r.push(e.file_name().to_string_lossy().to_lowercase());
        }
    } else if group.is_file() {
        let c4group = game_dir.join(if cfg!(windows) { "c4group.exe" } else { "c4group" });
        if let Ok(out) = std::process::Command::new(&c4group).arg(&group).arg("-l").output() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Some(name) = line.split_whitespace().next() {
                    let n = name.to_lowercase();
                    if n.ends_with(".wav") || n.ends_with(".ogg") || n.ends_with(".mp3") {
                        r.push(n);
                    }
                }
            }
        }
    }
    if let Ok(mut m) = GAME_SOUNDS.lock() {
        m.insert(game_dir.to_path_buf(), r.clone());
    }
    r
}

/// C4SoundSystem's matching: no extension -> .wav; '*' -> '?'; '?' = one character; case-insensitive.
fn prepare(name: &str) -> String {
    let has_ext = Path::new(name).extension().is_some();
    let n = if has_ext { name.to_string() } else { format!("{}.wav", name) };
    n.replace('*', "?").to_lowercase()
}

fn wildcard(pattern: &str, s: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (pattern.chars().collect(), s.chars().collect());
    p.len() == s.len() && p.iter().zip(&s).all(|(a, b)| *a == '?' || a == b)
}

/// Whether a Sound() name finds a file: Some(true) found, Some(false) not, None when we know no sounds at all.
/// The second value: the same name exists with another extension (the engine only appends .wav).
pub fn sound_found(index: &Index, name: &str) -> (Option<bool>, Option<String>) {
    let mut all: Vec<String> = index.sounds.iter().cloned().collect();
    if let Some(g) = &index.game_dir {
        all.extend(game_sounds(g));
    }
    if all.is_empty() {
        return (None, None);
    }
    let p = prepare(name);
    if all.iter().any(|s| wildcard(&p, s)) {
        return (Some(true), None);
    }
    let stem = p.rsplit_once('.').map(|(a, _)| a.to_string()).unwrap_or(p.clone());
    let other = all.iter().find(|s| s.rsplit_once('.').map(|(a, _)| wildcard(&stem, a)).unwrap_or(false)).cloned();
    (Some(false), other)
}

/// The folder of definition `id` as seen from `info` (its own folder for its own ID).
pub fn def_dir(index: &Index, info: &FileInfo, id: &str) -> Option<PathBuf> {
    if info.def_id.as_deref() == Some(id) {
        return info.path.parent().map(|p| p.to_path_buf());
    }
    index.id_for(id, &info.path).and_then(|i| i.defcore.parent().map(|p| p.to_path_buf()))
}

/// For SetAction/GetAction: Some(list of (id, its actions)) for the candidate definitions that have an ActMap;
/// None when none of them has one (nothing to check against).
pub fn candidate_actions(index: &Index, info: &FileInfo, ids: &[String]) -> Option<Vec<(String, Vec<String>)>> {
    let out: Vec<(String, Vec<String>)> = ids.iter()
        .filter_map(|id| def_dir(index, info, id).and_then(|d| actions(&d)).map(|a| (id.clone(), a)))
        .collect();
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sound_names_like_the_engine() {
        assert_eq!(prepare("Bow"), "bow.wav");
        assert_eq!(prepare("WoodHit*"), "woodhit?.wav");
        assert!(wildcard(&prepare("WoodHit*"), "woodhit1.wav"));
        assert!(!wildcard(&prepare("WoodHit*"), "woodhit12.wav"));
        assert!(wildcard(&prepare("Bell.ogg"), "bell.ogg"));
        assert!(!wildcard(&prepare("Bell"), "bell.ogg"));
    }
}
