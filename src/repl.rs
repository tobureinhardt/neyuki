// Interactive REPL: read-eval-print loop over a persistent VM.
// Line-oriented: type a chunk, see its result. Unfinished chunks
// (truncated constructs, unterminated strings) continue on `>> `.

use std::io::{self, BufRead, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Instant;

use crate::vm::machine::VM;

/// True when `err` looks like truncated input (hit EOF mid-construct)
/// rather than a finished-but-wrong chunk. Callers keep reading on true
/// and report the error on false. Leans toward continuing: a wrongly
/// continued chunk costs one more prompt line, while a wrongly reported
/// truncation breaks multi-line input entirely.
pub fn is_incomplete_input(err: &str) -> bool {
    err.contains("Unfinished")
        || err.contains("unfinished")
        || err.contains("got \"\"")
        || (err.contains("expected ") && !err.contains("got \""))
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else {
        "internal error".to_string()
    }
}

/// Compiles and runs one chunk on a persistent VM, returning the printed
/// line for value-producing chunks (`None` for statements). Globals (and
/// the standard library) survive across chunks. Uses the unchecked IR
/// pipeline on purpose: each chunk cannot be whole-program analyzed, so
/// unknown names resolve at runtime (nil when missing), exactly like the
/// Lua REPL. Even a panic from an interpreter bug surfaces as an error: an
/// interactive loop must never die on input.
pub fn eval_chunk(vm: &mut VM, source: &str) -> Result<Option<String>, String> {
    let run = || -> Result<Option<String>, String> {
        let proto =
            crate::compiler::compile_program(source, "repl", crate::compiler::Options::UNCHECKED)?;
        let val = vm.execute(proto)?;
        crate::vm::libs::task::run_due(vm);
        Ok(if val.is_nil() {
            None
        } else {
            Some(val.to_string())
        })
    };
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
        Ok(res) => res,
        Err(payload) => Err(panic_message(payload)),
    }
}

/// Runs the tasks that are due now, with the same panic guard as
/// `eval_chunk`.
fn run_due_tasks(vm: &mut VM) -> Result<(), String> {
    let run = || crate::vm::libs::task::run_due(vm);
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).map_err(panic_message)
}

/// Reads stdin lines on a separate thread so the scheduler can keep
/// running tasks while the prompt waits. The channel closes at EOF.
fn spawn_line_reader() -> Receiver<io::Result<String>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let stdin = io::stdin();
        loop {
            let mut line = String::new();
            let res = match stdin.lock().read_line(&mut line) {
                Ok(0) => return,
                Ok(_) => Ok(line),
                Err(e) => Err(e),
            };
            let failed = res.is_err();
            if tx.send(res).is_err() || failed {
                return;
            }
        }
    });
    rx
}

/// Waits for the next input line, running tasks as they fall due in the
/// meantime. `None` at EOF.
fn next_line(vm: &mut VM, lines: &Receiver<io::Result<String>>) -> Option<io::Result<String>> {
    loop {
        let Some(at) = crate::vm::libs::task::next_due(vm) else {
            return lines.recv().ok();
        };
        match lines.recv_timeout(at.saturating_duration_since(Instant::now())) {
            Ok(line) => return Some(line),
            Err(RecvTimeoutError::Disconnected) => return None,
            Err(RecvTimeoutError::Timeout) => {
                if let Err(e) = run_due_tasks(vm) {
                    eprintln!("Error: {}", e);
                }
            }
        }
    }
}

use crate::parser::readline::ReadLineEngine;

/// Runs the interactive loop on stdin/stdout until EOF.
pub fn run_repl() -> Result<(), String> {
    let lines = spawn_line_reader();
    let mut out = io::stdout();
    let mut vm = VM::new();
    let mut rl = ReadLineEngine::new();
    loop {
        write!(out, "{}", rl.prompt()).map_err(|e| format!("output error: {}", e))?;
        out.flush().map_err(|e| format!("output error: {}", e))?;
        let line = match next_line(&mut vm, &lines) {
            None => break,
            Some(Ok(line)) => line,
            Some(Err(e)) => return Err(format!("input error: {}", e)),
        };
        let status = rl.feed_line(&line);
        if matches!(
            status,
            crate::parser::readline::ChunkStatus::Incomplete { .. }
        ) {
            continue;
        }
        let chunk = rl.take_chunk();
        if chunk.trim().is_empty() {
            continue;
        }
        match eval_chunk(&mut vm, &chunk) {
            Ok(echo) => {
                if let Some(s) = echo {
                    writeln!(out, "{}", s).map_err(|e| format!("output error: {}", e))?;
                }
            }
            Err(e) if is_incomplete_input(&e) => {
                rl.feed_line(&chunk);
            }
            Err(e) => {
                writeln!(out, "Error: {}", e).map_err(|e| format!("output error: {}", e))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::value::Value;

    fn parse_err(src: &str) -> String {
        crate::compiler::compile_source(src).unwrap_err()
    }

    #[test]
    fn incomplete_truncated_inputs_continue() {
        for src in [
            "function f(",
            "if true then",
            "local x = {1, 2",
            "print(\"hi\"",
            "local x = 1 +",
            "f(",
        ] {
            assert!(
                is_incomplete_input(&parse_err(src)),
                "should continue: {:?}",
                src
            );
        }
    }

    #[test]
    fn complete_errors_report_immediately() {
        let src = "local = 1";
        assert!(
            !is_incomplete_input(&parse_err(src)),
            "should report: {:?}",
            src
        );
    }

    #[test]
    fn eval_chunk_reports_incomplete_with_rendered_diagnostic() {
        let mut vm = VM::new();
        let err = eval_chunk(&mut vm, "function f(").unwrap_err();
        assert!(
            is_incomplete_input(&err),
            "eval_chunk error must be recognized as incomplete input"
        );
    }

    #[test]
    fn eval_chunk_keeps_globals_across_chunks() {
        // Bare assignment writes the VM globals map, which outlives any
        // single chunk (unlike `global`, which is module-scoped).
        let mut vm = VM::new();
        eval_chunk(&mut vm, "repl_probe_xyz = 41").expect("chunk 1");
        let out = eval_chunk(&mut vm, "return repl_probe_xyz + 1").expect("chunk 2");
        assert_eq!(out.as_deref(), Some("42"));
    }

    #[test]
    fn eval_chunk_function_via_bare_assignment_persists() {
        // `name = function...` goes through SetGlobal, so it survives
        // across chunks. (The `function name...` statement form binds
        // chunk-local and does not persist: pre-existing semantics.)
        let mut vm = VM::new();
        eval_chunk(&mut vm, "f_repl_xyz = function(x)\nreturn x * 2\nend").expect("def");
        let out = eval_chunk(&mut vm, "return f_repl_xyz(21)").expect("call");
        assert_eq!(out.as_deref(), Some("42"));
    }

    #[test]
    fn tasks_run_while_waiting_for_input() {
        use std::sync::atomic::{AtomicBool, Ordering};

        static READY: AtomicBool = AtomicBool::new(false);
        READY.store(false, Ordering::SeqCst);
        fn mark_ready(_vm: &mut VM, _args: &[Value]) -> Result<Vec<Value>, String> {
            READY.store(true, Ordering::SeqCst);
            Ok(vec![Value::NIL])
        }

        let mut vm = VM::new();
        vm.globals.insert(
            "repl_ready_xyz".into(),
            crate::native!("repl_ready_xyz", mark_ready),
        );
        eval_chunk(
            &mut vm,
            "task.spawn(function()\n\
               while task.wait(0.005) do\n\
                 repl_ticks_xyz = (repl_ticks_xyz or 0) + 1\n\
                 if repl_delayed_xyz and repl_ticks_xyz >= 3 then repl_ready_xyz() end\n\
               end\n\
             end)\n\
             task.delay(0.01, function()\n\
               repl_delayed_xyz = true\n\
               if (repl_ticks_xyz or 0) >= 3 then repl_ready_xyz() end\n\
             end)",
        )
        .expect("spawn");
        let (tx, rx) = mpsc::channel();
        let typist = std::thread::spawn(move || {
            let deadline = Instant::now() + std::time::Duration::from_secs(5);
            while !READY.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            tx.send(Ok("next\n".to_string())).unwrap();
        });
        let line = next_line(&mut vm, &rx).expect("line").expect("ok");
        typist.join().unwrap();
        assert_eq!(line, "next\n");
        let out =
            eval_chunk(&mut vm, "return repl_delayed_xyz and repl_ticks_xyz >= 3").expect("check");
        assert_eq!(out.as_deref(), Some("true"));
    }

    #[test]
    fn eval_chunk_echo() {
        let mut vm = VM::new();
        assert_eq!(
            eval_chunk(&mut vm, "return 1 + 2")
                .expect("expr")
                .as_deref(),
            Some("3")
        );
        assert_eq!(eval_chunk(&mut vm, "local q = 1").expect("stmt"), None);
    }
}
