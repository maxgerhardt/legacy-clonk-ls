//! Command-line checker: index the roots and print diagnostics for C4Script files.
//!
//! Usage: c4check [--root DIR]... [--game-dir DIR] [--all-packs] [--no-type-checks] [--no-field-types] [--no-object-ids] [--languages DE,US] [--errors-only] [PATH]...
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
    let mut game_dir = std::env::var_os("C4LS_GAME_DIR").filter(|v| !v.is_empty()).map(PathBuf::from);
    let mut all_packs = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--root" => roots.push(PathBuf::from(args.next().expect("--root needs a folder"))),
            "--errors-only" => errors_only = true,
            "--game-dir" => game_dir = Some(PathBuf::from(args.next().expect("--game-dir needs a folder"))),
            "--all-packs" => all_packs = true,
            "--languages" => legacy_clonk_ls::analysis::strings::set_languages(
                args.next().expect("--languages needs a list like DE,US").split(',').map(|s| s.to_string()).collect()),
            "--no-type-checks" => legacy_clonk_ls::analysis::diagnostics::TYPE_CHECKS.store(false, std::sync::atomic::Ordering::Relaxed),
            "--no-field-types" => legacy_clonk_ls::analysis::diagnostics::FIELD_TYPES.store(false, std::sync::atomic::Ordering::Relaxed),
            "--no-object-ids" => legacy_clonk_ls::analysis::diagnostics::OBJECT_IDS.store(false, std::sync::atomic::Ordering::Relaxed),
            "-h" | "--help" => {
                eprintln!("usage: c4check [--root DIR]... [--game-dir DIR] [--all-packs] [--no-type-checks] [--no-field-types] [--no-object-ids] [--languages DE,US] [--errors-only] [PATH]...");
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
    eprintln!("checked {} files ({} indexed, {} ids): {} errors, {} warnings, {} infos",
        files.len(), index.files.len(), index.ids.len(), n_err, n_warn, n_info);
    if n_err > 0 { std::process::exit(1); }
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
