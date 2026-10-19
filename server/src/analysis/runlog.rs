//! Script errors from the engine's log (Clonk.log), so they show up at their source line.
//!
//! Formats (C4AulParse / C4AulExec through DebugLog):
//! - compile errors/warnings: `[error] [DebugLog] MSG (in Func, PATH:LINE:COL` or `... MSG (PATH:LINE:COL)`
//! - runtime errors: `[error] [DebugLog] MSG!` followed by the call chain, innermost first:
//!   `[info] [DebugLog]  by: Func(args) (obj ID #n) (PATH:LINE)`
//! The engine compiles definitions twice at startup, so identical entries are merged.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub func: String,
    pub path: PathBuf,
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunDiag {
    pub path: PathBuf,
    /// 1-based, as the engine prints it
    pub line: u32,
    pub col: Option<u32>,
    pub error: bool,
    pub message: String,
    /// true: raised while the script ran (with a call chain); false: when it was compiled
    pub runtime: bool,
    /// the calls that led there, innermost first (frames[0] is where it happened)
    pub frames: Vec<Frame>,
    /// the log's time stamp of the entry
    pub time: String,
}

/// (time, severity, message) of a `[time] [sev] [DebugLog] message` line.
fn debuglog(line: &str) -> Option<(&str, &str, &str)> {
    let rest = line.strip_prefix('[')?;
    let (time, rest) = rest.split_once("] [")?;
    let (sev, rest) = rest.split_once("] [DebugLog] ")?;
    Some((time, sev, rest))
}

/// `PATH:LINE:COL` or `PATH:LINE` at the end of a location (path may contain ':' as in C:\...).
fn location(s: &str) -> Option<(PathBuf, u32, Option<u32>)> {
    let s = s.trim().trim_end_matches(')');
    let idx = s.rfind(".c:")?;
    let path = &s[..idx + 2];
    let mut nums = s[idx + 3..].split(':');
    let line: u32 = nums.next()?.trim().parse().ok()?;
    let col: Option<u32> = nums.next().and_then(|c| c.trim().parse().ok());
    Some((PathBuf::from(path), line, col))
}

/// ` by: Func(args) (obj ID #n) (PATH:LINE)`
fn frame(msg: &str) -> Option<Frame> {
    let rest = msg.trim_start().strip_prefix("by: ")?;
    let func = rest.split('(').next()?.trim().to_string();
    let open = rest.rfind('(')?;
    let (path, line, _) = location(&rest[open + 1..])?;
    Some(Frame { func, path, line })
}

/// The script a log path means: the file itself, or for a script inside a packed group (Objects.c4d\...\Script.c
/// in the game folder) the indexed unpacked copy with the same group path.
pub fn resolve(path: &std::path::Path, index: &super::index::Index) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    let key = super::index::path_key(path);
    let parts: Vec<&str> = key.split('/').collect();
    let first = parts.iter().position(|p| [".c4d", ".c4f", ".c4s", ".c4g"].iter().any(|e| p.ends_with(e)))?;
    let suffix = format!("/{}", parts[first..].join("/"));
    index.files.values().find(|f| super::index::path_key(&f.path).ends_with(&suffix)).map(|f| f.path.clone())
}

/// LSP diagnostics of a log's entries, by script (source "Clonk.log"; code "runtime" for errors while running,
/// "engine" for the engine's compile errors).
pub fn diagnostics(entries: &[RunDiag], index: &super::index::Index) -> std::collections::HashMap<tower_lsp::lsp_types::Url, Vec<tower_lsp::lsp_types::Diagnostic>> {
    use tower_lsp::lsp_types::{Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, Location, NumberOrString, Position, Range};
    let mut out: std::collections::HashMap<tower_lsp::lsp_types::Url, Vec<Diagnostic>> = std::collections::HashMap::new();
    let at = |line: u32, col: Option<u32>| {
        let l = line.saturating_sub(1);
        let c = col.map(|c| c.saturating_sub(1)).unwrap_or(0);
        Range::new(Position::new(l, c), Position::new(l, if col.is_some() { c + 1 } else { 1000 }))
    };
    for e in entries {
        let Some(path) = resolve(&e.path, index) else { continue };
        let Some(uri) = super::index::path_to_uri(&path) else { continue };
        let related: Vec<DiagnosticRelatedInformation> = e.frames.iter().skip(1).filter_map(|f| {
            let p = resolve(&f.path, index)?;
            Some(DiagnosticRelatedInformation {
                location: Location::new(super::index::path_to_uri(&p)?, at(f.line, None)),
                message: format!("called by {}()", f.func),
            })
        }).collect();
        let what = if e.runtime { format!("while running {}()", e.frames.first().map(|f| f.func.as_str()).unwrap_or("?")) } else { "when compiling".to_string() };
        out.entry(uri).or_default().push(Diagnostic {
            range: at(e.line, e.col),
            severity: Some(if e.error { DiagnosticSeverity::ERROR } else { DiagnosticSeverity::WARNING }),
            code: Some(NumberOrString::String(if e.runtime { "runtime" } else { "engine" }.to_string())),
            source: Some("Clonk.log".to_string()),
            message: format!("{} (engine, {} at {})", e.message, what, e.time),
            related_information: (!related.is_empty()).then_some(related),
            ..Default::default()
        });
    }
    out
}

pub fn parse(text: &str) -> Vec<RunDiag> {
    let mut out: Vec<RunDiag> = vec![];
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let Some((time, sev, msg)) = debuglog(lines[i]) else { i += 1; continue };
        if !matches!(sev, "error" | "warning") {
            i += 1;
            continue;
        }
        // the call chain right after a runtime error
        let mut frames = vec![];
        let mut j = i + 1;
        while j < lines.len() {
            match debuglog(lines[j]).and_then(|(_, _, m)| frame(m)) {
                Some(f) => frames.push(f),
                None => break,
            }
            j += 1;
        }
        let entry = if let Some(first) = frames.first() {
            Some(RunDiag { path: first.path.clone(), line: first.line, col: None, error: sev == "error",
                message: msg.trim_end_matches('!').to_string(), runtime: true, frames: frames.clone(), time: time.to_string() })
        } else if let Some(open) = msg.rfind(" (") {
            // compile time: "MSG (in Func, PATH:L:C" / "MSG (PATH:L:C)"
            let loc = &msg[open + 2..];
            let loc = loc.split_once(", ").filter(|(a, _)| a.starts_with("in ")).map(|(_, b)| b).unwrap_or(loc);
            location(loc).map(|(path, line, col)| RunDiag { path, line, col, error: sev == "error",
                message: msg[..open].to_string(), runtime: false, frames: vec![], time: time.to_string() })
        } else {
            None
        };
        if let Some(e) = entry {
            if !out.iter().any(|o| o.path == e.path && o.line == e.line && o.message == e.message) {
                out.push(e);
            }
        }
        i = j.max(i + 1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_and_runtime_entries() {
        let log = r#"[2026-10-19 20:30:54.389] [error] [DebugLog] inherited disabled; use #strict syntax! (in f, C:\a\TS01.c4d\Script.c:1:29
[2026-10-19 20:30:55.027] [warning] [DebugLog] stupid func label: AB_D (C:\a\TS40.c4d\Script.c:3:10)
[2026-10-19 20:30:55.628] [error] [DebugLog] inherited disabled; use #strict syntax! (in f, C:\a\TS01.c4d\Script.c:1:29
[2026-10-19 20:30:56.696] [error] [DebugLog] Object call: No function "ControlDigDouble" in object "Feuerstein #19"!
[2026-10-19 20:30:56.713] [info] [DebugLog]  by: Check(Feuerstein #19) (obj TS57 #18) (C:\a\TS57.c4d\Script.c:5)
[2026-10-19 20:30:56.730] [info] [DebugLog]  by: Initialize() (obj TS57 #18) (C:\a\TS57.c4d\Script.c:6)
[2026-10-19 20:30:56.748] [info] [DebugLog]  by: Initialize() (C:\a\Script.c:18)
[2026-10-19 20:30:57.000] [info] Spiel gestartet.
"#;
        let d = parse(log);
        assert_eq!(d.len(), 3, "{:?}", d);
        assert_eq!((d[0].line, d[0].col, d[0].runtime, d[0].error), (1, Some(29), false, true));
        assert!(d[0].message.starts_with("inherited disabled"));
        assert_eq!((d[1].line, d[1].error), (3, false));
        assert_eq!(d[1].path, PathBuf::from(r"C:\a\TS40.c4d\Script.c"));
        assert!(d[2].runtime && d[2].line == 5 && d[2].frames.len() == 3 && d[2].frames[2].func == "Initialize");
        assert_eq!(d[2].message, "Object call: No function \"ControlDigDouble\" in object \"Feuerstein #19\"");
    }
}
