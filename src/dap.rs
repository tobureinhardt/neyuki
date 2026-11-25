//! Debug Adapter Protocol server for `neyuki dap`, over stdio.
//!
//! The session owns one VM and runs the program on the same thread, so
//! pausing is re-entrant: a breakpoint or finished step sends `stopped` and
//! serves requests from a nested loop until the client resumes. Line
//! breakpoints and stepping are checked from the VM's per-instruction trap,
//! which is only armed while there is something to check.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::rc::Rc;

use super::dap_json::{self, Json};
use crate::vm::machine::VM;
use crate::vm::value::{Value, ValueView, VmClosure};

#[derive(Clone, Copy, PartialEq, Eq)]
enum StepKind {
    /// Stop at the next new line in this frame or a caller.
    Over,
    /// Stop at the next new line anywhere.
    In,
    /// Stop at the next new line in a caller.
    Out,
}

#[derive(Clone, Copy)]
struct Step {
    kind: StepKind,
    depth: usize,
}

/// What a variables reference points at.
#[derive(Clone)]
enum VarRef {
    Locals(usize),
    Globals,
    Value(Value),
}

/// Debug session state, owned by the VM (`None` = no session).
pub struct DapSession {
    reader: Box<dyn BufRead>,
    writer: Box<dyn Write>,
    func_breakpoints: Vec<String>,
    /// Breakpoint lines by canonical source path.
    line_breakpoints: HashMap<String, Vec<u32>>,
    /// Canonical paths of the sources seen so far.
    canonical: HashMap<String, String>,
    step: Option<Step>,
    launch_program: Option<String>,
    stop_on_entry: bool,
    variables: HashMap<usize, VarRef>,
    next_var_ref: usize,
    next_seq: u64,
    terminated: bool,
}

impl DapSession {
    pub fn new(reader: Box<dyn BufRead>, writer: Box<dyn Write>) -> Self {
        Self {
            reader,
            writer,
            func_breakpoints: Vec::new(),
            line_breakpoints: HashMap::new(),
            canonical: HashMap::new(),
            step: None,
            launch_program: None,
            stop_on_entry: false,
            variables: HashMap::new(),
            next_var_ref: 1,
            next_seq: 1,
            terminated: false,
        }
    }

    pub fn stdio() -> Self {
        Self::new(
            Box::new(io::BufReader::new(io::stdin())),
            Box::new(io::stdout()),
        )
    }

    /// Whether the VM has to report every new line.
    pub fn wants_lines(&self) -> bool {
        self.step.is_some() || self.line_breakpoints.values().any(|l| !l.is_empty())
    }

    fn canonical_path(&mut self, path: &str) -> String {
        if let Some(found) = self.canonical.get(path) {
            return found.clone();
        }
        let canonical = std::fs::canonicalize(path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string());
        let canonical = if cfg!(windows) {
            canonical.to_lowercase()
        } else {
            canonical
        };
        self.canonical.insert(path.to_string(), canonical.clone());
        canonical
    }

    fn seq(&mut self) -> u64 {
        let n = self.next_seq;
        self.next_seq += 1;
        n
    }

    fn send(&mut self, message: Json) {
        let bytes = dap_json::render_json(&message).into_bytes();
        let _ = dap_json::write_message(&mut self.writer, &bytes);
    }

    fn respond(&mut self, request: &Json, body: Json) {
        let seq = self.seq();
        let message = Json::Obj(vec![
            ("seq".into(), Json::Num(seq as f64)),
            ("type".into(), Json::Str("response".into())),
            ("request_seq".into(), request_seq(request)),
            ("success".into(), Json::Bool(true)),
            ("command".into(), Json::Str(command_of(request).into())),
            ("body".into(), body),
        ]);
        self.send(message);
    }

    fn fail(&mut self, request: &Json, message: impl Into<String>) {
        let seq = self.seq();
        let message = Json::Obj(vec![
            ("seq".into(), Json::Num(seq as f64)),
            ("type".into(), Json::Str("response".into())),
            ("request_seq".into(), request_seq(request)),
            ("success".into(), Json::Bool(false)),
            ("command".into(), Json::Str(command_of(request).into())),
            ("message".into(), Json::Str(message.into())),
        ]);
        self.send(message);
    }

    fn event(&mut self, event: &str, body: Json) {
        let seq = self.seq();
        let message = Json::Obj(vec![
            ("seq".into(), Json::Num(seq as f64)),
            ("type".into(), Json::Str("event".into())),
            ("event".into(), Json::Str(event.into())),
            ("body".into(), body),
        ]);
        self.send(message);
    }

    fn register(&mut self, var: VarRef) -> usize {
        let id = self.next_var_ref;
        self.next_var_ref += 1;
        self.variables.insert(id, var);
        id
    }
}

fn request_seq(request: &Json) -> Json {
    Json::Num(request.get("seq").and_then(Json::as_f64).unwrap_or(0.0))
}

fn command_of(request: &Json) -> &str {
    request.get("command").and_then(Json::as_str).unwrap_or("")
}

fn obj(pairs: Vec<(&str, Json)>) -> Json {
    Json::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn session(vm: &mut VM) -> &mut DapSession {
    vm.dap
        .as_mut()
        .expect("DAP handlers run only while a debug session is attached")
}

/// Sends program output to the client instead of stdout, which carries the
/// protocol.
pub fn output(vm: &mut VM, category: &str, text: &str) {
    if let Some(s) = vm.dap.as_mut() {
        s.event(
            "output",
            obj(vec![
                ("category", Json::Str(category.into())),
                ("output", Json::Str(text.into())),
            ]),
        );
    }
}

fn preview(v: &Value) -> String {
    match v.view() {
        ValueView::String(s) => format!("{:?}", s.as_str()),
        ValueView::Table(t) => {
            let t = t.borrow();
            let named = t.fields_len() + t.general_len();
            match (t.array_len(), named) {
                (0, 0) => "{}".to_string(),
                (n, 0) => format!("{{...}} ({} items)", n),
                (0, m) => format!("{{...}} ({} keys)", m),
                (n, m) => format!("{{...}} ({} items, {} keys)", n, m),
            }
        }
        _ => v.to_string(),
    }
}

/// The source line a frame is executing.
fn frame_line(vm: &VM, index: usize) -> u32 {
    vm.frames
        .get(index)
        .and_then(|f| f.closure.proto.lines.get(f.ip.saturating_sub(1)).copied())
        .unwrap_or(0)
}

/// The variables visible in a frame: its upvalues, then its locals in
/// scope. A later name shadows an earlier one.
fn frame_variables(vm: &VM, index: usize) -> Vec<(String, Value)> {
    let Some(frame) = vm.frames.get(index) else {
        return Vec::new();
    };
    let proto = &frame.closure.proto;
    let pc = frame.ip.saturating_sub(1) as u32;
    let mut out: Vec<(String, Value)> = Vec::new();
    for (i, uv) in frame.closure.upvalues.iter().enumerate() {
        if let Some(name) = proto.upvalue_names.get(i).filter(|n| !n.is_empty()) {
            out.push((name.clone(), vm.upvalue_get(uv)));
        }
    }
    for info in &proto.local_names {
        if info.from_pc <= pc && pc < info.to_pc {
            let value = vm
                .stack
                .get(frame.base + info.reg as usize)
                .cloned()
                .unwrap_or(Value::NIL);
            out.retain(|(name, _)| *name != info.name);
            out.push((info.name.clone(), value));
        }
    }
    out
}

/// One request while the program is not running. Returns false once the
/// session should end.
fn handle(vm: &mut VM, request: &Json) -> bool {
    let args = request.get("arguments").cloned().unwrap_or(Json::Null);
    match command_of(request) {
        "initialize" => {
            let caps = obj(vec![
                ("supportsConfigurationDoneRequest", Json::Bool(true)),
                ("supportsFunctionBreakpoints", Json::Bool(true)),
                ("supportsEvaluateForHovers", Json::Bool(true)),
                ("supportsTerminateRequest", Json::Bool(true)),
            ]);
            let s = session(vm);
            s.respond(request, caps);
            s.event("initialized", obj(vec![]));
        }
        "launch" => {
            let program = args.get("program").and_then(Json::as_str).unwrap_or("");
            if program.is_empty() {
                session(vm).fail(request, "launch needs a program path");
                return true;
            }
            let s = session(vm);
            s.launch_program = Some(program.to_string());
            s.stop_on_entry = matches!(args.get("stopOnEntry"), Some(Json::Bool(true)));
            s.respond(request, obj(vec![]));
        }
        "setBreakpoints" => set_breakpoints(vm, request, &args),
        "setFunctionBreakpoints" => {
            let mut names = Vec::new();
            if let Some(Json::Arr(items)) = args.get("breakpoints") {
                for item in items {
                    if let Some(name) = item.get("name").and_then(Json::as_str) {
                        names.push(name.to_string());
                    }
                }
            }
            let verified = names
                .iter()
                .map(|_| obj(vec![("verified", Json::Bool(true))]))
                .collect();
            let s = session(vm);
            s.func_breakpoints = names;
            s.respond(request, obj(vec![("breakpoints", Json::Arr(verified))]));
        }
        "setExceptionBreakpoints" => {
            session(vm).respond(request, obj(vec![("breakpoints", Json::Arr(vec![]))]));
        }
        "configurationDone" => {
            session(vm).respond(request, obj(vec![]));
            let Some(program) = session(vm).launch_program.clone() else {
                return true;
            };
            if session(vm).stop_on_entry {
                session(vm).step = Some(Step {
                    kind: StepKind::In,
                    depth: 0,
                });
            }
            vm.update_trap();
            let code = match run_program(vm, &program) {
                Ok(()) => 0,
                Err(err) if err == TERMINATED => return false,
                Err(err) => {
                    let report = vm.describe_error(&err);
                    output(vm, "stderr", &format!("error: {}\n", report));
                    1
                }
            };
            let s = session(vm);
            s.event("exited", obj(vec![("exitCode", Json::Num(code as f64))]));
            s.event("terminated", obj(vec![]));
        }
        "threads" => threads(vm, request),
        "disconnect" | "terminate" => {
            session(vm).respond(request, obj(vec![]));
            return false;
        }
        _ => inspect(vm, request, &args),
    }
    !session(vm).terminated
}

/// Requests that inspect the program, answered the same way whether it is
/// paused or not.
fn inspect(vm: &mut VM, request: &Json, args: &Json) {
    match command_of(request) {
        "stackTrace" => stack_trace(vm, request, args),
        "scopes" => {
            let frame = args.get("frameId").and_then(Json::as_usize);
            match frame.filter(|f| *f < vm.frames.len()) {
                Some(frame) => {
                    let s = session(vm);
                    let locals = s.register(VarRef::Locals(frame));
                    let globals = s.register(VarRef::Globals);
                    let scopes = vec![
                        obj(vec![
                            ("name", Json::Str("Locals".into())),
                            ("presentationHint", Json::Str("locals".into())),
                            ("variablesReference", Json::Num(locals as f64)),
                            ("expensive", Json::Bool(false)),
                        ]),
                        obj(vec![
                            ("name", Json::Str("Globals".into())),
                            ("variablesReference", Json::Num(globals as f64)),
                            ("expensive", Json::Bool(true)),
                        ]),
                    ];
                    s.respond(request, obj(vec![("scopes", Json::Arr(scopes))]));
                }
                None => session(vm).fail(request, "unknown frame"),
            }
        }
        "variables" => {
            let reference = args
                .get("variablesReference")
                .and_then(Json::as_usize)
                .unwrap_or(0);
            let body = variables(vm, reference);
            session(vm).respond(request, body);
        }
        "evaluate" => {
            let expr = args.get("expression").and_then(Json::as_str).unwrap_or("");
            let frame = args.get("frameId").and_then(Json::as_usize);
            match evaluate(vm, expr, frame) {
                Ok(value) => {
                    let reference = child_ref(vm, &value);
                    let body = obj(vec![
                        ("result", Json::Str(preview(&value))),
                        ("type", Json::Str(value.type_name().into())),
                        ("variablesReference", Json::Num(reference as f64)),
                    ]);
                    session(vm).respond(request, body);
                }
                Err(err) => session(vm).fail(request, err),
            }
        }
        "pause" => session(vm).fail(request, "pausing a running program is not supported"),
        "continue" | "next" | "stepIn" | "stepOut" => {
            session(vm).fail(request, "the program is not paused")
        }
        other => {
            let message = format!("unsupported request '{}'", other);
            session(vm).fail(request, message)
        }
    }
}

fn threads(vm: &mut VM, request: &Json) {
    let thread = obj(vec![
        ("id", Json::Num(1.0)),
        ("name", Json::Str("main".into())),
    ]);
    session(vm).respond(request, obj(vec![("threads", Json::Arr(vec![thread]))]));
}

fn set_breakpoints(vm: &mut VM, request: &Json, args: &Json) {
    let path = args
        .get("source")
        .and_then(|s| s.get("path"))
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_string();
    let mut lines = Vec::new();
    if let Some(Json::Arr(items)) = args.get("breakpoints") {
        for item in items {
            if let Some(line) = item.get("line").and_then(Json::as_usize) {
                lines.push(line as u32);
            }
        }
    }
    let verified = lines
        .iter()
        .map(|line| {
            obj(vec![
                ("verified", Json::Bool(true)),
                ("line", Json::Num(*line as f64)),
            ])
        })
        .collect();
    let s = session(vm);
    let key = s.canonical_path(&path);
    s.line_breakpoints.insert(key, lines);
    s.respond(request, obj(vec![("breakpoints", Json::Arr(verified))]));
    vm.update_trap();
}

fn stack_trace(vm: &mut VM, request: &Json, args: &Json) {
    let start = args.get("startFrame").and_then(Json::as_usize).unwrap_or(0);
    let levels = args
        .get("levels")
        .and_then(Json::as_usize)
        .filter(|n| *n > 0)
        .unwrap_or(usize::MAX);
    let total = vm.frames.len();
    let mut frames = Vec::new();
    for index in (0..total).rev().skip(start).take(levels) {
        let proto = vm.frames[index].closure.proto.clone();
        let name = match proto.name.as_deref() {
            Some("main") | None => "main chunk".to_string(),
            Some(n) => n.to_string(),
        };
        let mut fields = vec![
            ("id", Json::Num(index as f64)),
            ("name", Json::Str(name)),
            ("line", Json::Num(frame_line(vm, index) as f64)),
            ("column", Json::Num(1.0)),
        ];
        if let Some(path) = proto.source.as_deref() {
            let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
            fields.push((
                "source",
                obj(vec![
                    ("name", Json::Str(file.into())),
                    ("path", Json::Str(path.into())),
                ]),
            ));
        }
        frames.push(obj(fields));
    }
    session(vm).respond(
        request,
        obj(vec![
            ("stackFrames", Json::Arr(frames)),
            ("totalFrames", Json::Num(total as f64)),
        ]),
    );
}

fn child_ref(vm: &mut VM, value: &Value) -> usize {
    if value.is_table() {
        session(vm).register(VarRef::Value(value.clone()))
    } else {
        0
    }
}

fn variables(vm: &mut VM, reference: usize) -> Json {
    let target = session(vm).variables.get(&reference).cloned();
    let pairs: Vec<(String, Value)> = match target {
        Some(VarRef::Locals(frame)) => frame_variables(vm, frame),
        Some(VarRef::Globals) => {
            let mut names: Vec<String> = vm.globals.keys().map(|k| k.to_string()).collect();
            names.sort();
            names
                .into_iter()
                .filter_map(|n| vm.globals.get(n.as_str()).cloned().map(|v| (n, v)))
                .collect()
        }
        Some(VarRef::Value(ref val)) if val.is_table() => {
            let t = val.table_borrow().unwrap();
            crate::vm::libs::table::entries_borrow(&t)
                .into_iter()
                .map(|(k, v)| {
                    let name = match k.as_str() {
                        Some(s) => s.to_string(),
                        None => format!("[{}]", preview(&k)),
                    };
                    (name, v)
                })
                .collect()
        }
        _ => Vec::new(),
    };
    let mut out = Vec::with_capacity(pairs.len());
    for (name, value) in pairs {
        let reference = child_ref(vm, &value);
        out.push(obj(vec![
            ("name", Json::Str(name)),
            ("value", Json::Str(preview(&value))),
            ("type", Json::Str(value.type_name().into())),
            ("variablesReference", Json::Num(reference as f64)),
        ]));
    }
    obj(vec![("variables", Json::Arr(out))])
}

/// Evaluates an expression, seeing the variables of `frame` when given.
/// The expression becomes the body of a function taking those variables
/// as parameters, so they are readable but assignments do not write back.
fn evaluate(vm: &mut VM, expr: &str, frame: Option<usize>) -> Result<Value, String> {
    if expr.trim().is_empty() {
        return Err("empty expression".to_string());
    }
    let visible = frame.map(|f| frame_variables(vm, f)).unwrap_or_default();
    let names: Vec<&str> = visible.iter().map(|(n, _)| n.as_str()).collect();
    let source = format!(
        "return function({}) return ({}) end",
        names.join(", "),
        expr
    );
    let proto =
        crate::compiler::compile_program(&source, "evaluate", crate::compiler::Options::UNCHECKED)?;
    let chunk = Value::closure(Rc::new(VmClosure {
        proto: Rc::new(proto),
        upvalues: Vec::new(),
    }));
    let args: Vec<Value> = visible.into_iter().map(|(_, v)| v).collect();
    vm.in_eval = true;
    let result = vm
        .call_function(chunk, &[])
        .and_then(|f| vm.call_function(f.into_iter().next().unwrap_or(Value::NIL), &args));
    vm.in_eval = false;
    vm.error_trace = None;
    Ok(result?.into_iter().next().unwrap_or(Value::NIL))
}

const TERMINATED: &str = "debugging terminated";

fn run_program(vm: &mut VM, path: &str) -> Result<(), String> {
    let source =
        std::fs::read_to_string(path).map_err(|e| format!("failed to read {}: {}", path, e))?;
    // Inlining would remove the very calls breakpoints and steps stop at.
    let proto = crate::compiler::compile_program(&source, path, crate::compiler::Options::DEBUG)?;
    vm.execute_program(proto).map(|_| ())
}

/// Called by the VM on every new line while `wants_lines` holds.
pub fn check_line(vm: &mut VM, line: u32) -> Result<(), String> {
    let depth = vm.frames.len();
    let source = vm
        .frames
        .last()
        .and_then(|f| f.closure.proto.source.clone());
    let s = session(vm);
    let stepped = match s.step {
        Some(Step {
            kind: StepKind::In, ..
        }) => true,
        Some(Step {
            kind: StepKind::Over,
            depth: from,
        }) => depth <= from,
        Some(Step {
            kind: StepKind::Out,
            depth: from,
        }) => depth < from,
        None => false,
    };
    let hit = match &source {
        Some(path) if !s.line_breakpoints.is_empty() => {
            let key = s.canonical_path(path);
            s.line_breakpoints
                .get(&key)
                .is_some_and(|lines| lines.contains(&line))
        }
        _ => false,
    };
    if stepped || hit {
        let reason = if hit { "breakpoint" } else { "step" };
        return pause(vm, reason);
    }
    Ok(())
}

/// Called by the VM when a function is entered.
pub(crate) fn check_break(vm: &mut VM, func_name: &str) -> Result<(), String> {
    if vm.in_eval {
        return Ok(());
    }
    let armed = vm
        .dap
        .as_ref()
        .is_some_and(|s| s.func_breakpoints.iter().any(|b| b == func_name));
    if armed {
        pause(vm, "function breakpoint")?;
    }
    Ok(())
}

/// Reports a stop and serves requests until the client resumes the
/// program, which returns Ok, or ends the session, which unwinds it.
fn pause(vm: &mut VM, reason: &str) -> Result<(), String> {
    {
        let s = session(vm);
        s.step = None;
        s.variables.clear();
        s.event(
            "stopped",
            obj(vec![
                ("reason", Json::Str(reason.into())),
                ("threadId", Json::Num(1.0)),
                ("allThreadsStopped", Json::Bool(true)),
            ]),
        );
    }
    vm.update_trap();
    loop {
        let raw = {
            let s = session(vm);
            match dap_json::read_message(&mut s.reader) {
                Ok(Some(bytes)) => bytes,
                Ok(None) | Err(_) => {
                    s.terminated = true;
                    return Err(TERMINATED.to_string());
                }
            }
        };
        let Ok(request) = dap_json::parse_json(&raw) else {
            continue;
        };
        let args = request.get("arguments").cloned().unwrap_or(Json::Null);
        let step = match command_of(&request) {
            "continue" => None,
            "next" => Some(StepKind::Over),
            "stepIn" => Some(StepKind::In),
            "stepOut" => Some(StepKind::Out),
            "disconnect" | "terminate" => {
                let s = session(vm);
                s.respond(&request, obj(vec![]));
                s.terminated = true;
                return Err(TERMINATED.to_string());
            }
            "setBreakpoints" => {
                set_breakpoints(vm, &request, &args);
                continue;
            }
            "threads" => {
                threads(vm, &request);
                continue;
            }
            _ => {
                inspect(vm, &request, &args);
                continue;
            }
        };
        let depth = vm.frames.len();
        let s = session(vm);
        s.step = step.map(|kind| Step { kind, depth });
        let body = if step.is_none() {
            obj(vec![("allThreadsContinued", Json::Bool(true))])
        } else {
            obj(vec![])
        };
        s.respond(&request, body);
        s.variables.clear();
        vm.update_trap();
        return Ok(());
    }
}

/// `neyuki dap`: serve one debug session on stdin and stdout.
pub fn run_dap_session() -> Result<(), String> {
    let mut vm = VM::new();
    vm.dap = Some(DapSession::stdio());
    vm.update_trap();
    serve(&mut vm);
    Ok(())
}

fn serve(vm: &mut VM) {
    loop {
        let raw = match dap_json::read_message(&mut session(vm).reader) {
            Ok(Some(bytes)) => bytes,
            Ok(None) | Err(_) => return,
        };
        let Ok(request) = dap_json::parse_json(&raw) else {
            continue;
        };
        if !handle(vm, &request) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn frame(json: &str) -> Vec<u8> {
        format!("Content-Length: {}\r\n\r\n{}", json.len(), json).into_bytes()
    }

    fn script(name: &str, source: &str) -> String {
        let mut path = std::env::temp_dir();
        path.push(format!("neyuki_dap_{}_{}.nyk", name, std::process::id()));
        std::fs::write(&path, source).unwrap();
        path.to_string_lossy().replace('\\', "\\\\")
    }

    /// Runs a whole session over the given requests and returns every
    /// message the server sent.
    fn converse(requests: &[String]) -> Vec<Json> {
        let mut input = Vec::new();
        for request in requests {
            input.extend(frame(request));
        }
        let out = Arc::new(Mutex::new(Vec::new()));
        let mut vm = VM::new();
        vm.dap = Some(DapSession::new(
            Box::new(io::Cursor::new(input)),
            Box::new(Shared(out.clone())),
        ));
        vm.update_trap();
        serve(&mut vm);
        let bytes = out.lock().unwrap().clone();
        let mut reader = io::BufReader::new(io::Cursor::new(bytes));
        let mut messages = Vec::new();
        while let Ok(Some(body)) = dap_json::read_message(&mut reader) {
            messages.push(dap_json::parse_json(&body).unwrap());
        }
        messages
    }

    fn find<'a>(messages: &'a [Json], kind: &str, name: &str) -> Vec<&'a Json> {
        messages
            .iter()
            .filter(|m| m.get("type").and_then(Json::as_str) == Some(kind))
            .filter(|m| {
                let key = if kind == "event" { "event" } else { "command" };
                m.get(key).and_then(Json::as_str) == Some(name)
            })
            .collect()
    }

    #[test]
    fn initialize_reports_capabilities_then_initialized() {
        let messages = converse(&[
            r#"{"seq":1,"type":"request","command":"initialize","arguments":{}}"#.to_string(),
        ]);
        let response = find(&messages, "response", "initialize")[0];
        assert_eq!(response.get("success"), Some(&Json::Bool(true)));
        assert!(!find(&messages, "event", "initialized").is_empty());
    }

    #[test]
    fn line_breakpoint_step_and_locals() {
        let path = script(
            "lines",
            "local a = 1\nlocal b = a + 1\nlocal c = b * 10\nprint(c)\n",
        );
        let messages = converse(&[
            r#"{"seq":1,"type":"request","command":"initialize","arguments":{}}"#.to_string(),
            format!(
                r#"{{"seq":2,"type":"request","command":"launch","arguments":{{"program":"{}"}}}}"#,
                path
            ),
            format!(
                r#"{{"seq":3,"type":"request","command":"setBreakpoints","arguments":{{"source":{{"path":"{}"}},"breakpoints":[{{"line":2}}]}}}}"#,
                path
            ),
            r#"{"seq":4,"type":"request","command":"configurationDone"}"#.to_string(),
            r#"{"seq":5,"type":"request","command":"next","arguments":{"threadId":1}}"#
                .to_string(),
            r#"{"seq":6,"type":"request","command":"stackTrace","arguments":{"threadId":1}}"#
                .to_string(),
            r#"{"seq":7,"type":"request","command":"scopes","arguments":{"frameId":0}}"#
                .to_string(),
            r#"{"seq":8,"type":"request","command":"variables","arguments":{"variablesReference":1}}"#
                .to_string(),
            r#"{"seq":9,"type":"request","command":"evaluate","arguments":{"expression":"b * 100","frameId":0}}"#
                .to_string(),
            r#"{"seq":10,"type":"request","command":"continue","arguments":{"threadId":1}}"#
                .to_string(),
        ]);
        let stops = find(&messages, "event", "stopped");
        assert_eq!(stops.len(), 2, "a breakpoint stop and a step stop");
        let reasons: Vec<_> = stops
            .iter()
            .map(|m| {
                m.get("body")
                    .unwrap()
                    .get("reason")
                    .unwrap()
                    .as_str()
                    .unwrap()
            })
            .collect();
        assert_eq!(reasons, ["breakpoint", "step"]);
        let trace = find(&messages, "response", "stackTrace")[0];
        let rendered = dap_json::render_json(trace);
        assert!(rendered.contains("\"line\":3"), "{}", rendered);
        let vars = dap_json::render_json(find(&messages, "response", "variables")[0]);
        assert!(vars.contains("\"a\"") && vars.contains("\"b\""), "{}", vars);
        let eval = dap_json::render_json(find(&messages, "response", "evaluate")[0]);
        assert!(eval.contains("\"200\""), "{}", eval);
        let output = dap_json::render_json(find(&messages, "event", "output")[0]);
        assert!(output.contains("20\\n"), "{}", output);
        assert!(!find(&messages, "event", "terminated").is_empty());
    }

    #[test]
    fn function_breakpoint_pauses_on_entry() {
        let path = script(
            "func",
            "local function add(x, y)\n  return x + y\nend\nlocal r = add(20, 22)\n",
        );
        let messages = converse(&[
            r#"{"seq":1,"type":"request","command":"initialize","arguments":{}}"#.to_string(),
            format!(
                r#"{{"seq":2,"type":"request","command":"launch","arguments":{{"program":"{}"}}}}"#,
                path
            ),
            r#"{"seq":3,"type":"request","command":"setFunctionBreakpoints","arguments":{"breakpoints":[{"name":"add"}]}}"#
                .to_string(),
            r#"{"seq":4,"type":"request","command":"configurationDone"}"#.to_string(),
            r#"{"seq":5,"type":"request","command":"stackTrace","arguments":{"threadId":1}}"#
                .to_string(),
            r#"{"seq":6,"type":"request","command":"continue","arguments":{"threadId":1}}"#
                .to_string(),
        ]);
        assert_eq!(find(&messages, "event", "stopped").len(), 1);
        let trace = dap_json::render_json(find(&messages, "response", "stackTrace")[0]);
        assert!(trace.contains("\"name\":\"add\""), "{}", trace);
    }

    #[test]
    fn errors_are_reported_as_output() {
        let path = script("err", "local t = nil\nprint(t.x)\n");
        let messages = converse(&[
            r#"{"seq":1,"type":"request","command":"initialize","arguments":{}}"#.to_string(),
            format!(
                r#"{{"seq":2,"type":"request","command":"launch","arguments":{{"program":"{}"}}}}"#,
                path
            ),
            r#"{"seq":3,"type":"request","command":"configurationDone"}"#.to_string(),
        ]);
        let output = dap_json::render_json(find(&messages, "event", "output")[0]);
        assert!(output.contains("index a nil value"), "{}", output);
        let exited = dap_json::render_json(find(&messages, "event", "exited")[0]);
        assert!(exited.contains("\"exitCode\":1"), "{}", exited);
    }
}
