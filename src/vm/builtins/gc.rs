//! `collectgarbage`.

use num_traits::ToPrimitive;

use crate::vm::machine::VM;
use crate::vm::value::Value;

pub fn builtin_collectgarbage(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let opt = args
        .first()
        .map(|v| v.to_string())
        .unwrap_or_else(|| "collect".to_string());
    match opt.as_str() {
        "collect" => match vm.gc_collect() {
            Ok(()) => Ok(vec![Value::small_int(0)]),
            Err(err) => Err(err),
        },
        "stop" => {
            vm.heap.old.stop();
            Ok(vec![Value::NIL])
        }
        "restart" => {
            vm.heap.old.restart();
            Ok(vec![Value::NIL])
        }
        "count" => Ok(vec![Value::float(vm.heap.old.count_kb())]),
        "isrunning" => Ok(vec![Value::bool(vm.heap.old.is_running)]),
        "step" => {
            let _step_size = args
                .get(1)
                .and_then(|v| v.as_int().and_then(|i| i.to_usize()))
                .unwrap_or(1024);
            // Performs one bounded collection step across pending Old/cycle work.
            vm.gc_auto_step()?;
            Ok(vec![Value::bool(vm.heap.old.has_pending_work())])
        }
        _ => Err(format!("unknown collectgarbage option '{}'", opt)),
    }
}
