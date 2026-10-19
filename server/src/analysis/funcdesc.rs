//! Function descriptions: `[Text|Image=ID:phase|Condition=Func|Method=Classic|Desc=Long text]` before a function
//! (context menu entries), split like the engine does (C4AulScriptFunc::ParseDesc in C4AulParse.cpp): the text up
//! to the first '|' is the menu text; then `key=value` segments: Image (an ID, `ID:phase` or `Contents`),
//! Condition (a function of the script or its #include'd scripts, GetFuncRecursive), Method (All, None, Classic,
//! JumpAndRun; anything else silently counts as All), Desc (a long description). Unknown keys are ignored.

/// What a piece of a description is (for highlighting, hover, completion and checks).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// the menu text or a Desc= value
    Text,
    /// $Name$ inside a text
    Translation,
    /// [ ] | =
    Punct,
    Key,
    /// Image=ID
    Id,
    /// Image=ID:phase
    Phase,
    /// Image=Contents, Method=Classic
    Constant,
    /// Condition=Func
    Function,
    /// the value of an unknown key
    Other,
}

/// A piece: byte range within the description's text, kind, and for values the key they belong to.
#[derive(Debug, Clone)]
pub struct Part {
    pub start: usize,
    pub end: usize,
    pub kind: Kind,
    pub key: String,
}

pub const KEYS: &[(&str, &str)] = &[
    ("Image", "picture of the menu entry: a definition ID, `ID:phase` (a picture phase) or `Contents`"),
    ("Condition", "a function of this script; the entry only shows when it returns true"),
    ("Method", "which control style shows the entry: All, None, Classic, JumpAndRun (anything else: All)"),
    ("Desc", "long description of the entry"),
];

pub const METHODS: &[&str] = &["All", "None", "Classic", "JumpAndRun"];

/// Text pieces with $Name$ string table references.
fn text_parts(text: &str, base: usize, kind: Kind, key: &str, out: &mut Vec<Part>) {
    let mut pos = 0;
    let bytes = text.as_bytes();
    while pos < text.len() {
        match text[pos..].find('$').map(|i| pos + i) {
            Some(open) => match text[open + 1..].find('$').map(|i| open + 1 + i) {
                Some(close) if close > open + 1 && text[open + 1..close].chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => {
                    if open > pos {
                        out.push(Part { start: base + pos, end: base + open, kind, key: key.to_string() });
                    }
                    out.push(Part { start: base + open, end: base + close + 1, kind: Kind::Translation, key: key.to_string() });
                    pos = close + 1;
                }
                _ => {
                    out.push(Part { start: base + pos, end: base + text.len(), kind, key: key.to_string() });
                    pos = text.len();
                }
            },
            None => {
                out.push(Part { start: base + pos, end: base + text.len(), kind, key: key.to_string() });
                pos = text.len();
            }
        }
        let _ = bytes;
    }
}

/// Split a description (the whole `[...]` text) into its pieces.
pub fn parts(desc: &str) -> Vec<Part> {
    let mut out = vec![];
    if !desc.starts_with('[') {
        return out;
    }
    out.push(Part { start: 0, end: 1, kind: Kind::Punct, key: String::new() });
    let closed = desc.ends_with(']') && desc.len() >= 2;
    let inner_end = if closed { desc.len() - 1 } else { desc.len() };
    let inner = &desc[1..inner_end];
    let mut base = 1;
    for (i, seg) in inner.split('|').enumerate() {
        if i > 0 {
            out.push(Part { start: base - 1, end: base, kind: Kind::Punct, key: String::new() });
        }
        if i == 0 {
            text_parts(seg, base, Kind::Text, "", &mut out);
        } else if let Some(eq) = seg.find('=') {
            let key = &seg[..eq];
            out.push(Part { start: base, end: base + eq, kind: Kind::Key, key: key.to_string() });
            out.push(Part { start: base + eq, end: base + eq + 1, kind: Kind::Punct, key: key.to_string() });
            let v = &seg[eq + 1..];
            let vb = base + eq + 1;
            match key {
                "Image" if v == "Contents" => out.push(Part { start: vb, end: vb + v.len(), kind: Kind::Constant, key: key.to_string() }),
                "Image" => match v.find(':') {
                    Some(c) => {
                        out.push(Part { start: vb, end: vb + c, kind: Kind::Id, key: key.to_string() });
                        out.push(Part { start: vb + c, end: vb + c + 1, kind: Kind::Punct, key: key.to_string() });
                        out.push(Part { start: vb + c + 1, end: vb + v.len(), kind: Kind::Phase, key: key.to_string() });
                    }
                    None => out.push(Part { start: vb, end: vb + v.len(), kind: Kind::Id, key: key.to_string() }),
                },
                "Condition" => out.push(Part { start: vb, end: vb + v.len(), kind: Kind::Function, key: key.to_string() }),
                "Method" => out.push(Part { start: vb, end: vb + v.len(), kind: Kind::Constant, key: key.to_string() }),
                "Desc" => text_parts(v, vb, Kind::Text, key, &mut out),
                _ => out.push(Part { start: vb, end: vb + v.len(), kind: Kind::Other, key: key.to_string() }),
            }
        } else if !seg.is_empty() {
            out.push(Part { start: base, end: base + seg.len(), kind: Kind::Other, key: String::new() });
        }
        base += seg.len() + 1;
    }
    if closed {
        out.push(Part { start: desc.len() - 1, end: desc.len(), kind: Kind::Punct, key: String::new() });
    }
    out.retain(|p| p.end > p.start);
    out
}

/// The piece at byte offset `at` of the description.
pub fn part_at(desc: &str, at: usize) -> Option<Part> {
    parts(desc).into_iter().find(|p| p.start <= at && at < p.end.max(p.start + 1))
}

/// Problems the engine passes over silently: (byte range, message).
pub fn problems(desc: &str, has_func: impl Fn(&str) -> bool, has_id: impl Fn(&str) -> bool) -> Vec<(usize, usize, String)> {
    let mut out = vec![];
    for p in parts(desc) {
        let v = &desc[p.start..p.end];
        match p.kind {
            Kind::Key if !KEYS.iter().any(|(k, _)| *k == v) => out.push((p.start, p.end,
                format!("unknown key \"{}\" in the function description (the engine ignores it; known: Image, Condition, Method, Desc)", v))),
            Kind::Function if !has_func(v) => out.push((p.start, p.end,
                format!("Condition={}: no function {} in this script (the engine then always shows the entry)", v, v))),
            Kind::Id if !has_id(v) => out.push((p.start, p.end, format!("Image={}: unknown definition ID (the entry gets no picture)", v))),
            Kind::Constant if p.key == "Method" && !METHODS.contains(&v) => out.push((p.start, p.end,
                format!("Method={}: unknown (the engine treats it as All; known: All, None, Classic, JumpAndRun)", v))),
            Kind::Phase if v.parse::<i32>().is_err() => out.push((p.start, p.end, format!("Image phase \"{}\" isn't a number", v))),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_like_the_engine() {
        let d = "[$Ctx$ text|Image=CXCN:2|Condition=HasKnowledge|Method=Classic|Foo=bar]";
        let kinds: Vec<(Kind, &str)> = parts(d).iter().map(|p| (p.kind, &d[p.start..p.end])).collect();
        assert!(kinds.contains(&(Kind::Translation, "$Ctx$")));
        assert!(kinds.contains(&(Kind::Text, " text")));
        assert!(kinds.contains(&(Kind::Id, "CXCN")));
        assert!(kinds.contains(&(Kind::Phase, "2")));
        assert!(kinds.contains(&(Kind::Function, "HasKnowledge")));
        assert!(kinds.contains(&(Kind::Constant, "Classic")));
        assert!(kinds.contains(&(Kind::Other, "bar")));
        let probs = problems(d, |f| f == "HasKnowledge", |i| i == "CXCN");
        assert_eq!(probs.len(), 1, "{:?}", probs);
        assert!(probs[0].2.contains("unknown key \"Foo\""));
        let probs = problems("[x|Condition=Nope|Method=Mouse|Image=ZZZZ]", |_| false, |_| false);
        assert_eq!(probs.len(), 3, "{:?}", probs);
    }
}
