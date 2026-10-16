//! Command-line checker: index the roots and print diagnostics for C4Script files.
//!
//! Usage: c4check [--root DIR]... [--errors-only] [PATH]...
//!   --root DIR     folder to index (repeatable; default: current directory)
//!   PATH           files or folders to check (default: everything indexed)
//! Output: <file>:<line>:<col>: <severity> [<code>] <message>; exit code 1 if there are errors.

use legacy_clonk_ls::analysis::{diagnostics::diagnose, index::{path_key, new_parser, extract, Index}, text::decode_bytes};
use std::path::PathBuf;
use tower_lsp::lsp_types::DiagnosticSeverity;

fn main() {
    let mut roots = vec![];
    let mut targets = vec![];
    let mut errors_only = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--root" => roots.push(PathBuf::from(args.next().expect("--root needs a folder"))),
            "--errors-only" => errors_only = true,
            "-h" | "--help" => {
                eprintln!("usage: c4check [--root DIR]... [--errors-only] [PATH]...");
                return;
            }
            _ => targets.push(PathBuf::from(a)),
        }
    }
    if roots.is_empty() {
        roots.push(std::env::current_dir().unwrap());
    }
    let roots: Vec<PathBuf> = roots.into_iter().map(|r| r.canonicalize().unwrap_or(r)).collect();
    let index = Index::build(roots);
    let targets: Vec<String> = targets.iter().map(|t| path_key(&t.canonicalize().unwrap_or(t.clone()))).collect();
    let mut files: Vec<_> = index.files.values().filter(|f| {
        let k = path_key(&f.path);
        targets.is_empty() || targets.iter().any(|t| k.starts_with(t.as_str()))
    }).collect();
    files.sort_by_key(|f| path_key(&f.path));

    let (mut n_err, mut n_warn, mut n_info) = (0, 0, 0);
    let mut parser = new_parser();
    for f in &files {
        let src = decode_bytes(&std::fs::read(&f.path).unwrap_or_default());
        let tree = match parser.parse(&src, None) { Some(t) => t, None => continue };
        let info = extract(&f.path, &src, &tree);
        for d in diagnose(&index, &info, &src, &tree) {
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
