//! Script names of the engine's debugger (C4AulScript::ScriptName, e.g. `C:\..\Foo.c4s\Script.c` or
//! `Objects.c4d\Items.c4d\Flint.c4d\Script.c` relative to the engine folder) <-> local files.

use std::path::{Path, PathBuf};

const GROUP_EXTS: [&str; 5] = [".c4d", ".c4f", ".c4s", ".c4g", ".c4p"];

fn is_group(component: &str) -> bool {
    let c = component.to_lowercase();
    GROUP_EXTS.iter().any(|e| c.ends_with(e))
}

/// Lowercase path components, `/` and `\` both separators.
pub fn components(path: &str) -> Vec<String> {
    path.split(['/', '\\']).filter(|c| !c.is_empty() && *c != ".").map(|c| c.to_lowercase()).collect()
}

/// The engine script a local file is: the same path, else the one sharing the most trailing components, where the
/// shared part must reach into a group (`Flint.c4d/Script.c`), so a bare `Script.c` never matches.
pub fn engine_name_for(local: &Path, engine_names: &[String]) -> Option<String> {
    let want = components(&local.to_string_lossy());
    let mut best: Option<(usize, &String)> = None;
    for name in engine_names {
        let have = components(name);
        if have == want {
            return Some(name.clone());
        }
        let shared = want.iter().rev().zip(have.iter().rev()).take_while(|(a, b)| a == b).count();
        if shared < 2 || !want[want.len() - shared..].iter().any(|c| is_group(c)) {
            continue;
        }
        if best.map_or(true, |(n, _)| shared > n) {
            best = Some((shared, name));
        }
    }
    best.map(|(_, n)| n.clone())
}

/// The local file of an engine script: the path itself (relative ones from the engine's working folder), or for a
/// script inside a packed group the language server's unpacked copy of that pack
/// (`%LOCALAPPDATA%/legacy-clonk-ls/packs/<pack>-<size>-<mtime>/<pack>/...`) if it exists.
pub fn local_path_for(engine_name: &str, engine_cwd: &Path) -> Option<PathBuf> {
    let direct = PathBuf::from(engine_name.replace('\\', std::path::MAIN_SEPARATOR_STR));
    let full = if direct.is_absolute() { direct } else { engine_cwd.join(direct) };
    if full.is_file() {
        return Some(full);
    }
    // the outermost group that is a file (packed): its cached unpacked copy
    let mut pack = PathBuf::new();
    let mut rest = vec![];
    let mut found = false;
    for c in full.components() {
        if found {
            rest.push(c.as_os_str().to_owned());
            continue;
        }
        pack.push(c);
        let s = c.as_os_str().to_string_lossy();
        if is_group(&s) && pack.is_file() {
            found = true;
        }
    }
    if !found {
        return None;
    }
    let (_, unpacked) = super::index::pack_cache_slot(&pack).ok()?;
    let local = rest.iter().fold(unpacked, |p, c| p.join(c));
    local.is_file().then_some(local)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_name_by_path_or_group_suffix() {
        let names = vec![
            r"C:\Game\Objects.c4d\Items.c4d\Flint.c4d\Script.c".to_string(),
            r"C:\Work\scenarios\Foo.c4s\Script.c".to_string(),
            r"C:\Work\scenarios\Foo.c4s\Bar.c4d\Script.c".to_string(),
            r"System.c4g\Helpers.c".to_string(),
        ];
        assert_eq!(engine_name_for(Path::new("c:/work/scenarios/foo.c4s/script.c"), &names).as_deref(), Some(names[1].as_str()));
        assert_eq!(engine_name_for(Path::new("C:/Cache/Objects.c4d-1-2/Objects.c4d/Items.c4d/Flint.c4d/Script.c"), &names).as_deref(), Some(names[0].as_str()));
        assert_eq!(engine_name_for(Path::new("D:/x/LegacyClonk/planet/System.c4g/Helpers.c"), &names).as_deref(), Some(names[3].as_str()));
        // only the file name in common: no match
        assert_eq!(engine_name_for(Path::new("D:/other/Script.c"), &names), None);
        // the longer shared suffix wins
        assert_eq!(engine_name_for(Path::new("E:/copy/Foo.c4s/Bar.c4d/Script.c"), &names).as_deref(), Some(names[2].as_str()));
    }
}
