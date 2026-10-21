//! Command-line checker: index the roots and print diagnostics for C4Script files.
//!
//! Usage: c4check [--root DIR]... [--game-dir DIR] [--all-packs] [--no-type-checks] [--no-field-types] [--no-object-ids] [--no-lint] [--runtime-log Clonk.log] [--nil-checks] [--create-may-fail] [--caller-inference] [--languages DE,US] [--errors-only] [PATH]...
//!   --root DIR     folder to index (repeatable; default: current directory)
//!   --game-dir DIR game folder (default: env C4LS_GAME_DIR): its Objects.c4d and the packs the scenarios
//!                  name in [Definitions] are indexed too (packed groups are unpacked once into a cache)
//!   --all-packs    index every pack of the game folder
//!   PATH           files or folders to check (default: everything indexed)
//! Output: <file>:<line>:<col>: <severity> [<code>] <message>; exit code 1 if there are errors.

use legacy_clonk_ls::analysis::{diagnostics::diagnose, index::{self, path_key, new_parser, extract, Index}, text::decode_bytes};
use std::path::PathBuf;
use tower_lsp::lsp_types::DiagnosticSeverity;

fn main() {
    let mut roots = vec![];
    let mut targets = vec![];
    let mut errors_only = false;
    let mut runtime_logs: Vec<PathBuf> = vec![];
    let mut game_dir = std::env::var_os("C4LS_GAME_DIR").filter(|v| !v.is_empty()).map(PathBuf::from);
    let mut all_packs = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--root" => roots.push(PathBuf::from(args.next().expect("--root needs a folder"))),
            "--errors-only" => errors_only = true,
            "--runtime-log" => runtime_logs.push(PathBuf::from(args.next().expect("--runtime-log needs a Clonk.log"))),
            "--game-dir" => game_dir = Some(PathBuf::from(args.next().expect("--game-dir needs a folder"))),
            "--all-packs" => all_packs = true,
            "--languages" => legacy_clonk_ls::analysis::strings::set_languages(
                args.next().expect("--languages needs a list like DE,US").split(',').map(|s| s.to_string()).collect()),
            "--no-type-checks" => legacy_clonk_ls::analysis::diagnostics::TYPE_CHECKS.store(false, std::sync::atomic::Ordering::Relaxed),
            "--no-field-types" => legacy_clonk_ls::analysis::diagnostics::FIELD_TYPES.store(false, std::sync::atomic::Ordering::Relaxed),
            "--no-object-ids" => legacy_clonk_ls::analysis::diagnostics::OBJECT_IDS.store(false, std::sync::atomic::Ordering::Relaxed),
            "--create-may-fail" => legacy_clonk_ls::analysis::diagnostics::ASSUME_CREATE.store(false, std::sync::atomic::Ordering::Relaxed),
            "--caller-inference" => legacy_clonk_ls::analysis::diagnostics::CALLER_INFERENCE.store(true, std::sync::atomic::Ordering::Relaxed),
            "--no-lint" => legacy_clonk_ls::analysis::diagnostics::LINT.store(false, std::sync::atomic::Ordering::Relaxed),
            "--nil-checks" => legacy_clonk_ls::analysis::diagnostics::NIL_CHECKS.store(true, std::sync::atomic::Ordering::Relaxed),
            "-h" | "--help" => {
                eprintln!("usage: c4check [--root DIR]... [--game-dir DIR] [--all-packs] [--no-type-checks] [--no-field-types] [--no-object-ids] [--no-lint] [--runtime-log Clonk.log] [--nil-checks] [--create-may-fail] [--caller-inference] [--languages DE,US] [--errors-only] [PATH]...");
                return;
            }
            _ => targets.push(PathBuf::from(a)),
        }
    }
    if roots.is_empty() {
        roots.push(std::env::current_dir().unwrap());
    }
    let roots: Vec<PathBuf> = roots.into_iter().map(|r| absolute(&r)).collect();
    let mut index = Index::build(roots);
    if let Some(gd) = &game_dir {
        let packs = index::game_packs_to_index(&index, gd, all_packs);
        let (packs_index, log) = index::index_game_packs(gd, &packs);
        for l in log {
            eprintln!("{}", l);
        }
        index.merge(packs_index);
    }
    let targets: Vec<String> = targets.iter().map(|t| path_key(&absolute(t))).collect();
    let mut files: Vec<_> = index.files.values().filter(|f| {
        let k = path_key(&f.path);
        targets.is_empty() || targets.iter().any(|t| k.starts_with(t.as_str()))
    }).collect();
    files.sort_by_key(|f| path_key(&f.path));

    // check on all cores (work queue: the scripts differ a lot in cost), print in file order
    let next = std::sync::atomic::AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 16);
    let mut results: Vec<(usize, Vec<tower_lsp::lsp_types::Diagnostic>)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads).map(|_| s.spawn(|| {
            let mut parser = new_parser();
            let mut out = vec![];
            loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(f) = files.get(i) else { break };
                let src = decode_bytes(&std::fs::read(&f.path).unwrap_or_default());
                let Some(tree) = parser.parse(&src, None) else { continue };
                let info = extract(&f.path, &src, &tree);
                out.push((i, diagnose(&index, &info, &src, &tree)));
            }
            out
        })).collect();
        handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    results.sort_by_key(|(i, _)| *i);

    let (mut n_err, mut n_warn, mut n_info) = (0, 0, 0);
    for (i, diags) in results {
        let f = files[i];
        for d in diags {
            let sev = d.severity.unwrap_or(DiagnosticSeverity::ERROR);
            let label = match sev {
                DiagnosticSeverity::ERROR => { n_err += 1; "error" }
                DiagnosticSeverity::WARNING => { n_warn += 1; "warning" }
                _ => { n_info += 1; "info" }
            };
            if errors_only && sev != DiagnosticSeverity::ERROR { continue; }
            let code = match &d.code { Some(tower_lsp::lsp_types::NumberOrString::String(s)) => s.clone(), _ => String::new() };
            println!("{}:{}:{}: {} [{}] {}", f.path.display(), d.range.start.line + 1, d.range.start.character + 1, label, code, d.message);
        }
    }
    // DefCore.txt / ActMap.txt / Scenario.txt below the paths given (or the roots): ini::check
    if lint_files() {
        let mut ini_files: Vec<PathBuf> = vec![];
        let starts: Vec<PathBuf> = if targets.is_empty() { index.roots.clone() } else { targets.iter().map(PathBuf::from).collect() };
        fn is_ini(name: &str) -> bool {
            ["DefCore.txt", "ActMap.txt", "Scenario.txt"].iter().any(|x| x.eq_ignore_ascii_case(name))
        }
        fn walk(d: &std::path::Path, out: &mut Vec<PathBuf>) {
            for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
                let p = e.path();
                let n = e.file_name().to_string_lossy().to_string();
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    if ![".git", "target", "node_modules", ".claude"].contains(&n.as_str()) {
                        walk(&p, out);
                    }
                } else if is_ini(&n) {
                    out.push(p);
                }
            }
        }
        for s in &starts {
            let s = std::path::Path::new(s);
            // a single file: only an INI file itself (a Script.c given directly isn't one: its function
            // descriptions [..] looked like unknown sections)
            let named_ini = s.file_name().map(|n| is_ini(&n.to_string_lossy())).unwrap_or(false);
            if s.is_dir() { walk(s, &mut ini_files) } else if s.is_file() && named_ini { ini_files.push(s.to_path_buf()) }
        }
        ini_files.sort();
        ini_files.dedup();
        for p in ini_files {
            let name = p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
            let kind = match name.as_str() {
                "defcore.txt" => legacy_clonk_ls::analysis::ini::IniFile::DefCore,
                "actmap.txt" => legacy_clonk_ls::analysis::ini::IniFile::ActMap,
                "scenario.txt" => legacy_clonk_ls::analysis::ini::IniFile::Scenario,
                _ => continue,
            };
            let text = decode_bytes(&std::fs::read(&p).unwrap_or_default());
            let ctx = legacy_clonk_ls::analysis::ini::Ctx { index: &index, path: &p, game_dir: game_dir.as_deref() };
            for d in legacy_clonk_ls::analysis::ini::check(kind, &text, &ctx) {
                let sev = d.severity.unwrap_or(DiagnosticSeverity::ERROR);
                let label = match sev {
                    DiagnosticSeverity::ERROR => { n_err += 1; "error" }
                    DiagnosticSeverity::WARNING => { n_warn += 1; "warning" }
                    _ => { n_info += 1; "info" }
                };
                if errors_only && sev != DiagnosticSeverity::ERROR { continue; }
                let code = match &d.code { Some(tower_lsp::lsp_types::NumberOrString::String(s)) => s.clone(), _ => String::new() };
                println!("{}:{}:{}: {} [{}] {}", p.display(), d.range.start.line + 1, d.range.start.character + 1, label, code, d.message);
            }
        }
    }
    // the engine's script errors from Clonk.log files (analysis/runlog.rs), after the static ones
    for log in &runtime_logs {
        let text = decode_bytes(&std::fs::read(log).unwrap_or_default());
        let entries = legacy_clonk_ls::analysis::runlog::parse(&text);
        let by_file = legacy_clonk_ls::analysis::runlog::diagnostics(&entries, &index);
        let mut rows: Vec<(String, u32, u32, String)> = by_file.iter().flat_map(|(uri, ds)| {
            let path = uri.to_file_path().map(|p| p.display().to_string()).unwrap_or_else(|_| uri.to_string());
            ds.iter().map(move |d| {
                let sev = if d.severity == Some(DiagnosticSeverity::ERROR) { "error" } else { "warning" };
                let code = match &d.code { Some(tower_lsp::lsp_types::NumberOrString::String(c)) => c.clone(), _ => String::new() };
                let chain: Vec<String> = d.related_information.iter().flatten().map(|r| format!("{} ({}:{})", r.message, r.location.uri.path().rsplit('/').next().unwrap_or(""), r.location.range.start.line + 1)).collect();
                let chain = if chain.is_empty() { String::new() } else { format!(" [{}]", chain.join(", ")) };
                (path.clone(), d.range.start.line + 1, d.range.start.character + 1, format!("{} [{}] {}{}", sev, code, d.message, chain))
            }).collect::<Vec<_>>()
        }).collect();
        rows.sort();
        for (path, line, col, rest) in rows {
            println!("{}:{}:{}: {}", path, line, col, rest);
        }
        eprintln!("{}: {} script errors/warnings from the engine", log.display(), entries.len());
    }
    eprintln!("checked {} files ({} indexed, {} ids): {} errors, {} warnings, {} infos",
        files.len(), index.files.len(), index.ids.len(), n_err, n_warn, n_info);
    if n_err > 0 { std::process::exit(1); }
}

/// DefCore/ActMap/Scenario.txt checks run with lint on (they are lint-like: the engine is silent about them).
fn lint_files() -> bool {
    legacy_clonk_ls::analysis::diagnostics::lint_enabled()
}

/// Canonical absolute path without the Windows verbatim prefix (\\?\) that canonicalize() adds.
fn absolute(p: &PathBuf) -> PathBuf {
    let c = p.canonicalize().unwrap_or_else(|_| p.clone());
    let s = c.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => c,
    }
}
