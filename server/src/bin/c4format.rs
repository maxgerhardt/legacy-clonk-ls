//! c4format: format C4Script files like clang-format, with the rules of the nearest `.c4format` file.
//!
//! usage: c4format [--check | -i | --verify] [--style FILE] PATH...
//!   (no flag)  print the formatted script (one file) to stdout
//!   --check    list the files that would change, exit code 1 if any
//!   -i         rewrite the files in place
//!   --verify   format every script, check idempotence (formatting twice changes nothing) and report why
//!              scripts are left unchanged (old syntax, syntax errors, safety check); exit code 1 on a failure
//! PATH: script files or folders (all *.c below them).

use legacy_clonk_ls::analysis::format::{format_explain, Style};
use legacy_clonk_ls::analysis::text::decode_bytes;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn scripts(p: &Path, out: &mut Vec<PathBuf>) {
    if p.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(p).into_iter().flatten().flatten().map(|e| e.path()).collect();
        entries.sort();
        for e in entries {
            scripts(&e, out);
        }
    } else if p.extension().map(|e| e.eq_ignore_ascii_case("c")).unwrap_or(false) {
        out.push(p.to_path_buf());
    }
}

/// Write the formatted text in the file's encoding (UTF-8/ASCII stays, Windows-1252 scripts stay Windows-1252).
fn write_back(f: &Path, original: &[u8], text: &str) {
    let bytes: Vec<u8> = if std::str::from_utf8(original).is_ok() {
        text.as_bytes().to_vec()
    } else {
        text.chars().map(|c| if (c as u32) < 256 { c as u8 } else { b'?' }).collect()
    };
    if let Err(e) = std::fs::write(f, bytes) {
        eprintln!("{}: {}", f.display(), e);
    }
}

fn main() {
    let mut mode = "print";
    let mut style_file: Option<PathBuf> = None;
    let mut paths = vec![];
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--check" => mode = "check",
            "-i" | "--in-place" => mode = "inplace",
            "--verify" => mode = "verify",
            "--tree" => mode = "tree",
            "--style" => style_file = Some(PathBuf::from(args.next().expect("--style needs a file"))),
            "-h" | "--help" => {
                eprintln!("usage: c4format [--check | -i | --verify] [--style FILE] PATH...");
                return;
            }
            _ => paths.push(PathBuf::from(a)),
        }
    }
    let fixed = style_file.map(|f| {
        let text = std::fs::read_to_string(&f).unwrap_or_else(|e| panic!("{}: {}", f.display(), e));
        Style::parse(&text).unwrap_or_else(|e| panic!("{}: {}", f.display(), e))
    });
    let mut files = vec![];
    for p in &paths {
        scripts(p, &mut files);
    }
    let (mut changed, mut failures) = (0, 0);
    let mut reasons: BTreeMap<&'static str, usize> = BTreeMap::new();
    for f in &files {
        let bytes = std::fs::read(f).unwrap_or_default();
        let src = decode_bytes(&bytes);
        let style = match &fixed {
            Some(s) => s.clone(),
            None => match Style::for_file(f) {
                Ok((s, _)) => s,
                Err(e) => {
                    eprintln!("{}", e);
                    std::process::exit(2);
                }
            },
        };
        if mode == "tree" {
            let tree = legacy_clonk_ls::analysis::index::new_parser().parse(&src, None).unwrap();
            println!("{}", tree.root_node().to_sexp());
            continue;
        }
        let result = format_explain(&src, &style);
        match mode {
            "print" => match &result {
                Ok(t) => print!("{}", t),
                Err(r) => {
                    eprintln!("{}: left unchanged: {}", f.display(), r);
                    print!("{}", src);
                }
            },
            "check" | "inplace" => {
                if let Ok(t) = &result {
                    if *t != src {
                        changed += 1;
                        if mode == "check" {
                            println!("{}", f.display());
                        } else {
                            write_back(f, &bytes, t);
                        }
                    }
                }
            }
            _ => match &result {
                Ok(t) => {
                    *reasons.entry("formatted").or_default() += 1;
                    if *t != src {
                        changed += 1;
                    }
                    match format_explain(t, &style) {
                        Ok(t2) if t2 == *t => {}
                        Ok(_) => {
                            failures += 1;
                            println!("NOT IDEMPOTENT {}", f.display());
                        }
                        Err(r) => {
                            failures += 1;
                            println!("SECOND RUN FAILED ({}) {}", r, f.display());
                        }
                    }
                }
                Err(r) => {
                    *reasons.entry(r).or_default() += 1;
                    if r.starts_with("safety") {
                        failures += 1;
                        println!("SAFETY ({}) {}", r, f.display());
                    }
                }
            },
        }
    }
    match mode {
        "check" => {
            eprintln!("{} of {} files would change", changed, files.len());
            std::process::exit(if changed > 0 { 1 } else { 0 });
        }
        "inplace" => eprintln!("{} of {} files changed", changed, files.len()),
        "verify" => {
            println!("{} files: {:?}; {} would change; {} failures", files.len(), reasons, changed, failures);
            std::process::exit(if failures > 0 { 1 } else { 0 });
        }
        _ => {}
    }
}
