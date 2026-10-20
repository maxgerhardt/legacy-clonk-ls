//! Debug adapter for C4Script (Debug Adapter Protocol over stdin/stdout): drives the engine's script debugger
//! (LegacyClonk with /debug:<port>, C4AulDebug in the fork's debugger branch).
//!
//! launch: starts the engine with `/debug:<port> /debugwait` (it waits after linking the scripts until the
//!   breakpoints are set); arguments `scenario`, `engine` (clonk.exe), `player`, `args`, `cwd`, `port`,
//!   `stopOnEntry`, `log` (Clonk.log to show in the debug console; default `<cwd>/Clonk.log`). A config.txt next to
//!   the engine is passed as /config: unless `args` has one.
//! attach: connects to a running engine: `port`, `cwd` (resolves relative script names), `log`.
//! Features: line breakpoints, break on script errors (exception filter "errors"), pause, step in/over/out,
//! call stack, scopes (local variables, the object's locals, statics), expandable arrays/maps/objects,
//! evaluate (also hover), engine log as debug console output. Scripts in packed groups open from the language
//! server's unpacked pack cache, else as read-only sources sent by the engine.

use legacy_clonk_ls::analysis::debugmap;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

enum Msg {
    Dap(Value),
    DapClosed,
    EngineEvent(String),
    EngineClosed,
    Log(String),
    Exited(i32),
}

/// Connection to the engine's debugger port: requests are answered in order; events go to the main loop.
struct Engine {
    stream: TcpStream,
    responses: Receiver<Result<String, String>>,
}

impl Engine {
    fn connect(port: u16, timeout: Duration, events: Sender<Msg>) -> Result<Engine, String> {
        let deadline = Instant::now() + timeout;
        let stream = loop {
            match TcpStream::connect(("127.0.0.1", port)) {
                Ok(s) => break s,
                Err(e) if Instant::now() > deadline => return Err(format!("could not connect to the engine's debugger port {}: {}", port, e)),
                Err(_) => std::thread::sleep(Duration::from_millis(250)),
            }
        };
        let (tx, responses) = mpsc::channel();
        let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
        std::thread::spawn(move || loop {
            let mut header = String::new();
            if reader.read_line(&mut header).map_or(true, |n| n == 0) {
                let _ = events.send(Msg::EngineClosed);
                return;
            }
            let mut parts = header.split_whitespace();
            let kind = parts.next().unwrap_or("").to_string();
            let len: usize = parts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
            let mut body = vec![0u8; len];
            if reader.read_exact(&mut body).is_err() {
                let _ = events.send(Msg::EngineClosed);
                return;
            }
            let body = String::from_utf8_lossy(&body).to_string();
            let _ = match kind.as_str() {
                "EVT" => events.send(Msg::EngineEvent(body)).map_err(|_| ()),
                "OK" => tx.send(Ok(body)).map_err(|_| ()),
                _ => tx.send(Err(body)).map_err(|_| ()),
            };
        });
        Ok(Engine { stream, responses })
    }

    fn request(&mut self, command: &str, payload: &str) -> Result<String, String> {
        let msg = format!("{} {}\n{}", command, payload.len(), payload);
        self.stream.write_all(msg.as_bytes()).map_err(|e| e.to_string())?;
        self.responses.recv_timeout(Duration::from_secs(30)).map_err(|_| format!("the engine did not answer '{}'", command))?
    }
}

/// What a variablesReference stands for: `vars <frame> <scope> <path>...`
#[derive(Clone)]
struct VarRef {
    frame: usize,
    scope: u8,
    path: Vec<usize>,
}

struct Adapter {
    seq: i64,
    out: std::io::Stdout,
    events: Sender<Msg>,
    engine: Option<Engine>,
    child: Option<std::sync::Arc<std::sync::Mutex<std::process::Child>>>,
    cwd: PathBuf,
    scripts: Vec<String>,
    var_refs: HashMap<i64, VarRef>,
    next_ref: i64,
    source_refs: Vec<String>,
    stop_on_entry: bool,
    started: bool,
    terminated: bool,
}

impl Adapter {
    fn send(&mut self, mut msg: Value) {
        self.seq += 1;
        msg["seq"] = json!(self.seq);
        let body = msg.to_string();
        let mut out = self.out.lock();
        let _ = write!(out, "Content-Length: {}\r\n\r\n{}", body.len(), body);
        let _ = out.flush();
    }

    fn respond(&mut self, req: &Value, body: Value) {
        self.send(json!({"type": "response", "request_seq": req["seq"], "success": true, "command": req["command"], "body": body}));
    }

    fn fail(&mut self, req: &Value, message: &str) {
        self.send(json!({"type": "response", "request_seq": req["seq"], "success": false, "command": req["command"], "message": message,
            "body": {"error": {"id": 1, "format": message, "showUser": false}}}));
    }

    fn event(&mut self, name: &str, body: Value) {
        self.send(json!({"type": "event", "event": name, "body": body}));
    }

    fn output(&mut self, category: &str, text: &str) {
        self.event("output", json!({"category": category, "output": text}));
    }

    fn engine(&mut self) -> Result<&mut Engine, String> {
        self.engine.as_mut().ok_or_else(|| "not connected to the engine".to_string())
    }

    fn request(&mut self, command: &str, payload: &str) -> Result<String, String> {
        self.engine()?.request(command, payload)
    }

    fn new_ref(&mut self, r: VarRef) -> i64 {
        self.next_ref += 1;
        self.var_refs.insert(self.next_ref, r);
        self.next_ref
    }

    fn source_for(&mut self, engine_name: &str) -> Value {
        if let Some(p) = debugmap::local_path_for(engine_name, &self.cwd) {
            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            return json!({"name": name, "path": p.to_string_lossy()});
        }
        let id = match self.source_refs.iter().position(|s| s == engine_name) {
            Some(i) => i + 1,
            None => {
                self.source_refs.push(engine_name.to_string());
                self.source_refs.len()
            }
        };
        let comps = debugmap::components(engine_name);
        let name = engine_name.rsplit(['/', '\\']).nth(1).map(|g| format!("{} ({})", engine_name.rsplit(['/', '\\']).next().unwrap_or(""), g))
            .unwrap_or_else(|| comps.last().cloned().unwrap_or_default());
        json!({"name": name, "sourceReference": id, "origin": "engine (packed group)", "presentationHint": "deemphasize"})
    }

    fn connected(&mut self, req: &Value) {
        match self.request("scripts", "") {
            Ok(s) => self.scripts = s.lines().map(|l| l.to_string()).collect(),
            Err(e) => {
                self.fail(req, &e);
                return;
            }
        }
        self.respond(req, json!({}));
        self.event("initialized", json!({}));
    }

    fn launch(&mut self, req: &Value) {
        let args = &req["arguments"];
        let Some(scenario) = args["scenario"].as_str() else { return self.fail(req, "launch needs \"scenario\" (path of the .c4s)") };
        let Some(engine) = args["engine"].as_str().filter(|e| !e.is_empty()) else {
            return self.fail(req, "launch needs \"engine\" (path of clonk.exe; the VS Code extension fills it from legacy-clonk-ext.pathToGameExecutable)");
        };
        let engine = PathBuf::from(engine);
        self.cwd = args["cwd"].as_str().map(PathBuf::from).or_else(|| engine.parent().map(Path::to_path_buf)).unwrap_or_default();
        let port = args["port"].as_u64().map(|p| p as u16).unwrap_or_else(free_port);
        self.stop_on_entry = args["stopOnEntry"].as_bool().unwrap_or(false);
        let mut cmd = std::process::Command::new(&engine);
        // native separators: with '/' in the path the engine skips the definitions of enclosing .c4f folders
        cmd.arg(scenario.replace(['/', '\\'], std::path::MAIN_SEPARATOR_STR));
        if let Some(p) = args["player"].as_str().filter(|p| !p.is_empty()) {
            cmd.arg(p);
        }
        cmd.args(["/nonetwork", "/console", &format!("/debug:{}", port), "/debugwait"]);
        let extra: Vec<&str> = args["args"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
        cmd.args(&extra);
        // an engine folder with its own config.txt (like the tools' engine/ folders) uses it
        let config = engine.parent().map(|d| d.join("config.txt"));
        if let Some(config) = config.filter(|c| c.is_file()) {
            if !extra.iter().any(|a| a.to_lowercase().starts_with("/config:")) {
                cmd.arg(format!("/config:{}", config.display()));
            }
        }
        cmd.current_dir(&self.cwd).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        let log = args["log"].as_str().map(PathBuf::from).unwrap_or_else(|| self.cwd.join("Clonk.log"));
        let started = std::time::SystemTime::now();
        let child = match cmd.spawn() {
            Ok(c) => std::sync::Arc::new(std::sync::Mutex::new(c)),
            Err(e) => return self.fail(req, &format!("could not start {}: {}", engine.display(), e)),
        };
        self.child = Some(child.clone());
        let events = self.events.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(300));
            let status = child.lock().unwrap().try_wait();
            match status {
                Ok(Some(s)) => {
                    let _ = events.send(Msg::Exited(s.code().unwrap_or(0)));
                    return;
                }
                Ok(None) => {}
                Err(_) => return,
            }
        });
        tail_log(log, Some(started), self.events.clone());
        match Engine::connect(port, Duration::from_secs(90), self.events.clone()) {
            Ok(e) => self.engine = Some(e),
            Err(e) => {
                self.kill();
                return self.fail(req, &format!("{} - the engine needs the script debugger (/debug:<port>, LegacyClonk fork branch \"debugger\"; set legacy-clonk-ext.debugEngine)", e));
            }
        }
        self.output("console", &format!("Started {} {} (debugger port {})\n", engine.display(), scenario, port));
        self.connected(req);
    }

    fn attach(&mut self, req: &Value) {
        let args = &req["arguments"];
        let Some(port) = args["port"].as_u64() else { return self.fail(req, "attach needs \"port\" (the engine's /debug:<port>)") };
        self.cwd = args["cwd"].as_str().map(PathBuf::from).unwrap_or_default();
        if let Some(log) = args["log"].as_str() {
            tail_log(PathBuf::from(log), None, self.events.clone());
        }
        match Engine::connect(port as u16, Duration::from_secs(10), self.events.clone()) {
            Ok(e) => self.engine = Some(e),
            Err(e) => return self.fail(req, &e),
        }
        self.started = true;
        self.connected(req);
    }

    fn set_breakpoints(&mut self, req: &Value) {
        let args = &req["arguments"];
        let path = args["source"]["path"].as_str().unwrap_or("").to_string();
        let lines: Vec<i64> = args["breakpoints"].as_array().map(|b| b.iter().filter_map(|x| x["line"].as_i64()).collect()).unwrap_or_default();
        let mut name = debugmap::engine_name_for(Path::new(&path), &self.scripts);
        if name.is_none() {
            if let Ok(s) = self.request("scripts", "") {
                self.scripts = s.lines().map(|l| l.to_string()).collect();
            }
            name = debugmap::engine_name_for(Path::new(&path), &self.scripts);
        }
        let Some(name) = name else {
            let bps: Vec<Value> = lines.iter().map(|l| json!({"verified": false, "line": l, "message": "this script isn't loaded by the running scenario"})).collect();
            return self.respond(req, json!({"breakpoints": bps}));
        };
        let payload = std::iter::once(name.clone()).chain(lines.iter().map(|l| l.to_string())).collect::<Vec<_>>().join("\n");
        match self.request("break", &payload) {
            Ok(r) => {
                let actual: HashMap<i64, i64> = r.lines().filter_map(|l| {
                    let (a, b) = l.split_once('\t')?;
                    Some((a.parse().ok()?, b.parse().ok()?))
                }).collect();
                let bps: Vec<Value> = lines.iter().map(|l| {
                    let a = actual.get(l).copied().unwrap_or(0);
                    if a > 0 { json!({"verified": true, "line": a}) } else { json!({"verified": false, "line": l, "message": "no code on or after this line"}) }
                }).collect();
                self.respond(req, json!({"breakpoints": bps}));
            }
            Err(e) => self.fail(req, &e),
        }
    }

    fn stack_trace(&mut self, req: &Value) {
        let text = match self.request("stack", "") {
            Ok(t) => t,
            Err(e) => return self.fail(req, &e),
        };
        let mut frames = vec![];
        for (i, l) in text.lines().enumerate() {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() < 4 {
                continue;
            }
            let name = if f[3].is_empty() { f[0].to_string() } else { format!("{}  [{}]", f[0], f[3]) };
            let line: i64 = f[2].parse().unwrap_or(0).max(1);
            let mut frame = json!({"id": i + 1, "name": name, "line": line, "column": 1});
            if !f[1].is_empty() {
                frame["source"] = self.source_for(f[1]);
            }
            frames.push(frame);
        }
        let n = frames.len();
        self.respond(req, json!({"stackFrames": frames, "totalFrames": n}));
    }

    fn scopes(&mut self, req: &Value) {
        let frame = req["arguments"]["frameId"].as_u64().unwrap_or(1).saturating_sub(1) as usize;
        let locals = self.new_ref(VarRef { frame, scope: 0, path: vec![] });
        let object = self.new_ref(VarRef { frame, scope: 1, path: vec![] });
        let statics = self.new_ref(VarRef { frame, scope: 2, path: vec![] });
        self.respond(req, json!({"scopes": [
            {"name": "Locals", "presentationHint": "locals", "variablesReference": locals, "expensive": false},
            {"name": "This object", "variablesReference": object, "expensive": false},
            {"name": "Statics", "variablesReference": statics, "expensive": true},
        ]}));
    }

    fn variables(&mut self, req: &Value) {
        let id = req["arguments"]["variablesReference"].as_i64().unwrap_or(0);
        let Some(r) = self.var_refs.get(&id).cloned() else { return self.respond(req, json!({"variables": []})) };
        let payload = std::iter::once(r.frame.to_string()).chain(std::iter::once(r.scope.to_string())).chain(r.path.iter().map(|p| p.to_string()))
            .collect::<Vec<_>>().join(" ");
        let text = match self.request("vars", &payload) {
            Ok(t) => t,
            Err(e) => return self.fail(req, &e),
        };
        let mut vars = vec![];
        for (i, l) in text.lines().enumerate() {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() < 4 {
                continue;
            }
            let children: usize = f[3].parse().unwrap_or(0);
            let reference = if children > 0 {
                let mut path = r.path.clone();
                path.push(i);
                self.new_ref(VarRef { frame: r.frame, scope: r.scope, path })
            } else {
                0
            };
            let mut v = json!({"name": f[0], "value": f[2], "type": f[1], "variablesReference": reference});
            if f[1] == "array" {
                v["indexedVariables"] = json!(children);
            }
            vars.push(v);
        }
        self.respond(req, json!({"variables": vars}));
    }

    fn evaluate(&mut self, req: &Value) {
        let args = &req["arguments"];
        let expr = args["expression"].as_str().unwrap_or("").trim().to_string();
        let frame = args["frameId"].as_u64().unwrap_or(1).saturating_sub(1) as usize;
        match self.request("eval", &format!("{} {}", frame, expr)) {
            Ok(t) => {
                let f: Vec<&str> = t.split('\t').collect();
                if f.len() < 4 {
                    return self.fail(req, &t);
                }
                let children: usize = f[2].parse().unwrap_or(0);
                let reference = if children > 0 {
                    self.new_ref(VarRef { frame, scope: 3, path: vec![f[3].parse().unwrap_or(0)] })
                } else {
                    0
                };
                self.respond(req, json!({"result": f[1], "type": f[0], "variablesReference": reference}));
            }
            Err(e) => self.fail(req, &e),
        }
    }

    fn resume(&mut self, req: &Value, command: &str) {
        self.var_refs.clear();
        match self.request(command, "") {
            Ok(_) => {
                let body = if command == "continue" { json!({"allThreadsContinued": true}) } else { json!({}) };
                self.respond(req, body)
            }
            Err(e) => self.fail(req, &e),
        }
    }

    fn kill(&mut self) {
        if let Some(c) = self.child.take() {
            let _ = c.lock().unwrap().kill();
        }
    }

    fn handle(&mut self, req: Value) -> bool {
        let command = req["command"].as_str().unwrap_or("").to_string();
        match command.as_str() {
            "initialize" => {
                self.respond(&req, json!({
                    "supportsConfigurationDoneRequest": true,
                    "supportsEvaluateForHovers": true,
                    "supportsTerminateRequest": true,
                    "exceptionBreakpointFilters": [{"filter": "errors", "label": "Script errors", "default": true,
                        "description": "Stop where a script error is raised, before the call stack unwinds"}],
                }));
            }
            "launch" => self.launch(&req),
            "attach" => self.attach(&req),
            "setBreakpoints" => self.set_breakpoints(&req),
            "setExceptionBreakpoints" => {
                let on = req["arguments"]["filters"].as_array().is_some_and(|f| f.iter().any(|x| x == "errors"));
                match self.request("breakerrors", if on { "1" } else { "0" }) {
                    Ok(_) => self.respond(&req, json!({})),
                    Err(e) => self.fail(&req, &e),
                }
            }
            "configurationDone" => {
                if self.stop_on_entry {
                    let _ = self.request("pause", "");
                }
                if !self.started {
                    self.started = true;
                    let _ = self.request("start", "");
                }
                self.respond(&req, json!({}));
            }
            "threads" => self.respond(&req, json!({"threads": [{"id": 1, "name": "Script"}]})),
            "stackTrace" => self.stack_trace(&req),
            "scopes" => self.scopes(&req),
            "variables" => self.variables(&req),
            "evaluate" => self.evaluate(&req),
            "continue" => self.resume(&req, "continue"),
            "next" => self.resume(&req, "next"),
            "stepIn" => self.resume(&req, "stepin"),
            "stepOut" => self.resume(&req, "stepout"),
            "pause" => match self.request("pause", "") {
                Ok(_) => self.respond(&req, json!({})),
                Err(e) => self.fail(&req, &e),
            },
            "source" => {
                let id = req["arguments"]["sourceReference"].as_u64().unwrap_or(0) as usize;
                let name = self.source_refs.get(id.wrapping_sub(1)).cloned();
                match name.map(|n| self.request("source", &n)) {
                    Some(Ok(text)) => self.respond(&req, json!({"content": text, "mimeType": "text/x-c4script"})),
                    Some(Err(e)) => self.fail(&req, &e),
                    None => self.fail(&req, "unknown source"),
                }
            }
            "terminate" => {
                self.kill();
                self.respond(&req, json!({}));
            }
            "disconnect" => {
                let _ = self.request("detach", "");
                let terminate = req["arguments"]["terminateDebuggee"].as_bool().unwrap_or(true);
                if terminate {
                    self.kill();
                }
                self.respond(&req, json!({}));
                return false;
            }
            _ => self.fail(&req, &format!("unsupported request {}", command)),
        }
        true
    }

    fn engine_event(&mut self, text: &str) {
        let f: Vec<&str> = text.split('\t').collect();
        match f[0] {
            "stopped" => {
                self.var_refs.clear();
                let reason = match f.get(1).copied().unwrap_or("") {
                    "breakpoint" => "breakpoint",
                    "step" => "step",
                    "pause" => "pause",
                    "error" => "exception",
                    other => other,
                }
                .to_string();
                let message = f.get(2).copied().unwrap_or("").to_string();
                let mut body = json!({"reason": reason, "threadId": 1, "allThreadsStopped": true});
                if reason == "exception" {
                    body["description"] = json!("Script error");
                    body["text"] = json!(message);
                    self.output("stderr", &format!("Script error: {}\n", message));
                }
                self.event("stopped", body);
            }
            "continued" => self.event("continued", json!({"threadId": 1, "allThreadsContinued": true})),
            _ => {}
        }
    }

    fn end(&mut self, exit_code: Option<i32>) {
        if self.terminated {
            return;
        }
        self.terminated = true;
        if let Some(c) = exit_code {
            self.event("exited", json!({"exitCode": c}));
        }
        self.event("terminated", json!({}));
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0)).ok().and_then(|l| l.local_addr().ok()).map(|a| a.port()).unwrap_or(4877)
}

/// Sends the lines the engine appends to its log (it rewrites the file on every start: with `newer_than`, the file
/// is read from its start once it was written after that time).
fn tail_log(path: PathBuf, newer_than: Option<std::time::SystemTime>, events: Sender<Msg>) {
    std::thread::spawn(move || {
        let mut pos: u64 = 0;
        let mut waiting = newer_than.is_some();
        if newer_than.is_none() {
            pos = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        }
        let mut partial = String::new();
        loop {
            std::thread::sleep(Duration::from_millis(250));
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            if waiting {
                match (meta.modified(), newer_than) {
                    (Ok(m), Some(t)) if m >= t => waiting = false,
                    _ => continue,
                }
            }
            if meta.len() < pos {
                pos = 0;
            }
            if meta.len() == pos {
                continue;
            }
            let Ok(mut f) = std::fs::File::open(&path) else { continue };
            use std::io::Seek;
            if f.seek(std::io::SeekFrom::Start(pos)).is_err() {
                continue;
            }
            let mut buf = vec![];
            if f.read_to_end(&mut buf).is_err() {
                continue;
            }
            pos += buf.len() as u64;
            partial.push_str(&legacy_clonk_ls::analysis::text::decode_bytes(&buf));
            if let Some(end) = partial.rfind('\n') {
                let complete: String = partial.drain(..=end).collect();
                if events.send(Msg::Log(complete)).is_err() {
                    return;
                }
            }
        }
    });
}

fn read_dap(events: Sender<Msg>) {
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut input = BufReader::new(stdin.lock());
        loop {
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                if input.read_line(&mut line).map_or(true, |n| n == 0) {
                    let _ = events.send(Msg::DapClosed);
                    return;
                }
                let line = line.trim();
                if line.is_empty() {
                    break;
                }
                if let Some(v) = line.strip_prefix("Content-Length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            if input.read_exact(&mut body).is_err() {
                let _ = events.send(Msg::DapClosed);
                return;
            }
            if let Ok(v) = serde_json::from_slice::<Value>(&body) {
                if events.send(Msg::Dap(v)).is_err() {
                    return;
                }
            }
        }
    });
}

fn main() {
    let (tx, rx) = mpsc::channel();
    read_dap(tx.clone());
    let mut a = Adapter {
        seq: 0,
        out: std::io::stdout(),
        events: tx,
        engine: None,
        child: None,
        cwd: PathBuf::new(),
        scripts: vec![],
        var_refs: HashMap::new(),
        next_ref: 0,
        source_refs: vec![],
        stop_on_entry: false,
        started: false,
        terminated: false,
    };
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Dap(req) => {
                if req["type"] == "request" && !a.handle(req) {
                    break;
                }
            }
            Msg::DapClosed => {
                a.kill();
                break;
            }
            Msg::EngineEvent(e) => a.engine_event(&e),
            Msg::EngineClosed => {
                a.engine = None;
                a.end(None);
            }
            Msg::Exited(code) => {
                a.engine = None;
                a.end(Some(code));
            }
            Msg::Log(text) => {
                for line in text.lines() {
                    let category = if line.contains("] [error] ") { "stderr" } else { "stdout" };
                    a.output(category, &format!("{}\n", line));
                }
            }
        }
    }
}
