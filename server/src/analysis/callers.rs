//! What a function's callers guarantee (switch: diagnostics::caller_inference_enabled).
//!
//! A function whose callers are all known: `private`/`protected` (or an engine callback with a verified guarantee),
//! its name never in a string (Schedule("F()"), Call("F"), ActMap StartCall=F, DefCore TimerCall=F: unknown callers)
//! and not another engine callback. Then
//! - a parameter has the union of the argument types at every indexed call site (+ the engine's guarantee), e.g.
//!   `private func Hit2(obj)` only called as `Hit2(this)` -> obj is an object;
//! - a call fact (`Contained()` isn't nil, types.rs CALL_FACT) holds at its start when it holds at every call site
//!   and every call is a direct call on the same object (`F()` / `this->F()`); `Entrance` starts contained.
//! Callers are analysed with this inference off (one level), so results don't depend on evaluation order.

use super::index::{named_children, path_key, FileInfo, FuncDef, Index};
use super::text::node_text;
use super::types::{FuncTypes, Ty, ANY, NIL, OBJECT};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Mutex;

/// Engine callbacks (C4Script.h PSF_*, plus the literal Call("~Selection") / Call("~ControlContents")); `{}` = any
/// text. Called with arguments we can't see, unless ENGINE_GUARANTEES says otherwise.
const CALLBACKS: &[&str] = &["Script{}", "Initialize", "InitializeDef", "Construction", "Destruction", "ContentsDestruction",
    "InitializePlayer", "InitializeScriptPlayer", "PreInitializePlayer", "RemovePlayer", "OnGameOver", "Hit", "Hit2", "Hit3",
    "Grab", "Grabbed", "RejectGrabbed", "Get", "Put", "Collection", "Collection2", "Ejection", "Entrance", "Departure",
    "Completion", "Purchase", "Sale", "Damage", "Incineration", "IncinerationEx", "Death", "ActivateEntrance", "Activate",
    "LiftTop", "Control{}", "Contained{}", "ControlUpdate", "ContainedUpdate", "Contact{}", "ControlCommand",
    "ControlCommandFinished", "DeepBreath", "CatchBlow", "QueryCatchBlow", "Stuck", "RejectCollect", "RejectContents",
    "GrabLost", "LineBreak", "BuildNeedsMaterial", "ControlTransfer", "UpdateTransferZone", "CalcValue", "CalcDefValue",
    "SellTo", "InputCallback", "MenuQueryCancel", "IsFulfilled", "IsFulfilledforPlr", "RejectEntrance", "RejectFight",
    "AttachTargetLost", "CrewSelection", "GetObject2Drop", "OnMenuSelection", "OnActionJump", "CalcBuyValue",
    "CalcSellValue", "MouseSelection", "OnOwnerChanged", "Recruitment", "Fx{}", "FireMode", "Selection", "ControlContents"];

/// (callback, parameter): the engine always passes an object there (C4Object.cpp: Entrance/Departure/Collection2/
/// Ejection/ContentsDestruction 1539-1607/251, Collection 5722 + C4ObjectCom.cpp 621, ActivateEntrance 1645,
/// Contained<Com> 3254/3298: sf->Exec(Contained, {C4VObj(this)}), ContainedUpdate 3260/3302).
const ENGINE_GUARANTEES: &[(&str, usize)] = &[("Entrance", 0), ("Departure", 0), ("Collection", 0), ("Collection2", 0),
    ("Ejection", 0), ("ContentsDestruction", 0), ("ActivateEntrance", 0), ("Contained{}", 0), ("ContainedUpdate", 0)];

fn matches(pattern: &str, name: &str) -> bool {
    match pattern.split_once("{}") {
        Some((pre, post)) => name.len() > pre.len() + post.len() && name.starts_with(pre) && name.ends_with(post),
        None => pattern == name,
    }
}

pub fn is_callback(name: &str) -> bool {
    CALLBACKS.iter().any(|p| matches(p, name))
}

fn engine_guarantee(name: &str, i: usize) -> bool {
    ENGINE_GUARANTEES.iter().any(|(p, j)| *j == i && matches(p, name))
}

thread_local! {
    /// > 0 while callers are analysed: no caller inference inside (one level, order-independent)
    static OFF: Cell<u32> = const { Cell::new(0) };
}

pub fn active() -> bool {
    super::diagnostics::caller_inference_enabled() && OFF.with(|o| o.get() == 0)
}

/// What the callers of a function guarantee.
#[derive(Clone, Default)]
pub struct CallerFacts {
    /// per parameter: the union of the argument types (None: unknown)
    pub params: Vec<Option<Ty>>,
    /// call facts holding at the start (types.rs CALL_FACT keys)
    pub facts: Vec<String>,
}

lazy_static::lazy_static! {
    static ref CACHE: Mutex<HashMap<String, CallerFacts>> = Mutex::new(HashMap::new());
}

pub fn clear_cache() {
    if let Ok(mut c) = CACHE.lock() {
        c.clear();
    }
}

/// The facts for function `f` of `file` (cached; unknown = empty).
pub fn facts(index: &Index, file: &FileInfo, src: &str, f: &FuncDef) -> CallerFacts {
    let key = format!("{}|{}|{}", path_key(&file.path), f.name, f.name_range.start.line);
    if let Some(r) = CACHE.lock().ok().and_then(|c| c.get(&key).cloned()) {
        return r;
    }
    OFF.with(|o| o.set(o.get() + 1));
    let r = compute(index, file, src, f);
    OFF.with(|o| o.set(o.get() - 1));
    if let Ok(mut c) = CACHE.lock() {
        c.insert(key, r.clone());
    }
    r
}

/// Procedures whose ExecAction ends the action without a target before the phase advance, so a PhaseCall always
/// runs with GetActionTarget() set (C4Object.cpp ExecAction: BUILD (not for structures) 5030, PUSH 5075, PULL 5134,
/// CHOP 5214, FIGHT 5235, LIFT 5278, ATTACH 5330, CONNECT 5382 (both targets)). StartCall/EndCall also run from
/// SetAction without a target: no guarantee.
const TARGET_PROCEDURES: &[&str] = &["PUSH", "PULL", "CHOP", "FIGHT", "LIFT", "ATTACH", "CONNECT"];

/// What the engine guarantees when it calls `f` (None: the engine may call it with no guarantee, e.g. an
/// unverified callback or an ActMap StartCall): (parameter types, call facts). Some((vec![], vec![])) when the engine
/// doesn't call it at all.
fn engine_side(index: &Index, file: &FileInfo, f: &FuncDef) -> Option<(Vec<Option<Ty>>, Vec<String>)> {
    let n = f.params.len();
    let mut params: Vec<Option<Ty>> = vec![Some(super::types::NONE); n];
    let mut facts: Option<Vec<String>> = None;
    let mut meet = |p: Vec<Option<Ty>>, k: Vec<String>, params: &mut Vec<Option<Ty>>| {
        for (a, b) in params.iter_mut().zip(p) {
            *a = match (*a, b) { (Some(x), Some(y)) => Some(x | y), _ => None };
        }
        facts = Some(match facts.take() { None => k, Some(prev) => prev.into_iter().filter(|x| k.contains(x)).collect() });
    };
    if is_callback(&f.name) {
        let p: Vec<Option<Ty>> = (0..n).map(|i| engine_guarantee(&f.name, i).then_some(OBJECT)).collect();
        let mut k = vec![];
        if f.name == "Entrance" {
            k.push(super::types::call_fact("Contained()"));
        }
        if f.name == "InitializePlayer" {
            if let (Some(plr), true) = (f.params.first(), scenario_places_crew(file)) {
                for c in [format!("GetCrew({})", plr.1), format!("GetCrew({},0)", plr.1), format!("GetHiRank({})", plr.1)] {
                    k.push(super::types::call_fact(&c));
                }
            } else if !ENGINE_GUARANTEES.iter().any(|(p, _)| matches(p, &f.name)) {
                return None;
            }
        } else if !ENGINE_GUARANTEES.iter().any(|(p, _)| matches(p, &f.name)) {
            return None;
        }
        meet(p, k, &mut params);
    }
    // an ActMap/DefCore calls functions of its own definition: uses in its definition family only
    let family: std::collections::HashSet<String> = index.family(file).iter().map(|x| path_key(&x.path)).chain(std::iter::once(path_key(&file.path))).collect();
    for (key, procedure) in index.ini_uses(&f.name).into_iter().filter(|(k, _, _)| family.contains(k)).map(|(_, a, b)| (a, b)) {
        if key != "PhaseCall" || !TARGET_PROCEDURES.contains(&procedure.as_str()) {
            return None;
        }
        let mut k = vec![super::types::call_fact("GetActionTarget()"), super::types::call_fact("GetActionTarget(0)")];
        if procedure == "CONNECT" {
            k.push(super::types::call_fact("GetActionTarget(1)"));
        }
        // the engine calls PhaseCall functions without parameters
        meet(vec![Some(NIL); n], k, &mut params);
    }
    Some((params, facts.unwrap_or_default()))
}

/// A scenario script whose Scenario.txt gives every player a crew (C4Player::PlaceReadyCrew before
/// InitializePlayer: Clonks= evaluated within its Min, default 1,0,1,10; or a Crew= list).
fn scenario_places_crew(file: &FileInfo) -> bool {
    let Some(dir) = file.path.parent() else { return false };
    if !dir.extension().map(|e| e.eq_ignore_ascii_case("c4s")).unwrap_or(false) {
        return false;
    }
    let text = std::fs::read(dir.join("Scenario.txt")).map(|b| super::text::decode_bytes(&b)).unwrap_or_default();
    let mut section = String::new();
    let mut ok = [true; 4];
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            section = line.trim_matches(|c| c == '[' || c == ']').to_string();
            continue;
        }
        let Some(i) = section.strip_prefix("Player").and_then(|n| n.parse::<usize>().ok()).filter(|i| (1..=4).contains(i)) else { continue };
        if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                // Std,Rnd,Min,Max: at least Min (and Std when Rnd is 0)
                "Clonks" => {
                    let v: Vec<i64> = v.split(',').map(|x| x.trim().parse().unwrap_or(0)).collect();
                    let min = v.get(2).copied().unwrap_or(0).max(if v.get(1).copied().unwrap_or(0) == 0 { v.first().copied().unwrap_or(0) } else { 0 });
                    if min < 1 {
                        ok[i - 1] = false;
                    }
                }
                "Crew" if !v.trim().is_empty() => ok[i - 1] = true,
                _ => {}
            }
        }
    }
    ok.iter().all(|x| *x)
}

/// The calls that can reach `f` of `file`: obj->F() from anywhere, direct calls from scripts that see `file`
/// (its definition family; any script for a global func).
fn relevant_sites<'a>(index: &'a Index, file: &FileInfo, f: &FuncDef) -> Vec<(&'a FileInfo, &'a super::index::CallSite)> {
    let global = f.visibility.as_deref() == Some("global");
    // a scenario script is no object: obj->F() never reaches it (only its own calls and GameCall strings)
    let scenario = file.def_id.is_none() && file.path.parent().and_then(|d| d.extension()).map(|e| e.eq_ignore_ascii_case("c4s")).unwrap_or(false);
    let family: std::collections::HashSet<String> = index.family(file).iter().map(|x| path_key(&x.path)).chain(std::iter::once(path_key(&file.path))).collect();
    index.call_sites(&f.name).into_iter().filter(|(cf, c)| (c.arrow && !scenario) || global || family.contains(&path_key(&cf.path))).collect()
}

/// The engine's guarantees alone (no caller analysis): when no script calls `f` itself.
pub fn engine_facts(index: &Index, file: &FileInfo, f: &FuncDef) -> CallerFacts {
    let n = f.params.len();
    let none = CallerFacts { params: vec![None; n], facts: vec![] };
    if !relevant_sites(index, file, f).is_empty() || index.named_in_strings(file, &f.name) {
        return none;
    }
    match engine_side(index, file, f) {
        Some((params, facts)) => CallerFacts { params: params.into_iter().map(|p| p.filter(|t| *t != super::types::NONE && *t != ANY)).collect(), facts },
        None => none,
    }
}

fn compute(index: &Index, file: &FileInfo, src: &str, f: &FuncDef) -> CallerFacts {
    let n = f.params.len();
    let unknown = CallerFacts { params: vec![None; n], facts: vec![] };
    let engine_called = is_callback(&f.name) || {
        let family: std::collections::HashSet<String> = index.family(file).iter().map(|x| path_key(&x.path)).chain(std::iter::once(path_key(&file.path))).collect();
        index.ini_uses(&f.name).iter().any(|(k, _, _)| family.contains(k))
    };
    let hidden = matches!(f.visibility.as_deref(), Some("private") | Some("protected"));
    if !engine_called && !hidden {
        return unknown;
    }
    if index.named_in_strings(file, &f.name) {
        return unknown;
    }
    let Some((engine_params, engine_facts)) = engine_side(index, file, f) else { return unknown };
    // an overload elsewhere (#include/#appendto) may call this one through inherited(...) with its own arguments
    let family: std::collections::HashSet<String> = index.family(file).iter().map(|x| path_key(&x.path)).collect();
    for (g_file, g) in index.all_funcs(&f.name) {
        if (path_key(&g_file.path) == path_key(&file.path) && g.name_range == f.name_range) || !family.contains(&path_key(&g_file.path)) {
            continue;
        }
        let (a, b) = (g.range.start.line, g.range.end.line);
        if g_file.calls.iter().any(|c| matches!(c.name.as_str(), "inherited" | "_inherited") && c.range.start.line >= a && c.range.start.line <= b) {
            return unknown;
        }
    }
    // the engine's own calls
    let mut params = engine_params;
    let mut facts: Option<Vec<String>> = if engine_called { Some(engine_facts) } else { None };
    let current = path_key(&file.path);
    for (site_file, site) in relevant_sites(index, file, f) {
        let (caller, caller_src): (&FileInfo, Option<&str>) = if path_key(&site_file.path) == current { (file, Some(src)) } else { (site_file, None) };
        let Some((types, call, args)) = site_types(caller, caller_src, &current, src, site.byte, index) else { return unknown };
        // arguments
        for (i, p) in params.iter_mut().enumerate() {
            let t = match args.get(i) {
                Some(a) => types.at.get(a).copied().unwrap_or(ANY),
                None => NIL,
            };
            *p = p.map(|old| old | t);
        }
        // call facts: only a direct call (or this->F()) runs on the same object
        let same_object = !site.arrow || call.1;
        let here: Vec<String> = if same_object { types.site_facts.get(&call.0).cloned().unwrap_or_default() } else { vec![] };
        facts = Some(match facts {
            None => here,
            Some(prev) => prev.into_iter().filter(|k| here.contains(k)).collect(),
        });
    }
    let params = params.into_iter().map(|p| p.filter(|t| *t != super::types::NONE && *t != ANY)).collect();
    CallerFacts { params, facts: facts.unwrap_or_default() }
}

/// Types of the function around the call whose name starts at `byte` in `caller`: (types, (call node id, receiver
/// is `this`), argument node ids).
fn site_types(caller: &FileInfo, caller_src: Option<&str>, current: &str, current_src: &str, byte: usize, index: &Index)
    -> Option<(FuncTypes, (usize, bool), Vec<usize>)> {
    let _ = caller_src; // the open document's text comes through `current`
    let entry = super::types::parsed_script(&caller.path, Some((current, current_src)))?;
    let (src, tree) = (&entry.0, &entry.1);
    let root = tree.root_node();
    let mut n = root.descendant_for_byte_range(byte, byte + 1)?;
    while n.kind() != "method_call" {
        n = n.parent()?;
    }
    let func = super::objids::enclosing_function(&n)?;
    let strict = super::diagnostics::strict_level(&root, src);
    let types = super::types::infer_function(index, caller, src, func, strict);
    let args: Vec<usize> = n.child_by_field_name("args").map(|a| named_children(&a)).unwrap_or_default()
        .into_iter().filter(|c| c.kind() != "comment").map(|c| c.id()).collect();
    let this_receiver = n.parent().filter(|p| p.kind() == "arrow_expression")
        .and_then(|p| named_children(&p).into_iter().next())
        .map(|t| t.kind() == "identifier" && node_text(&t, src) == "this").unwrap_or(false);
    Some((types, (n.id(), this_receiver), args))
}
