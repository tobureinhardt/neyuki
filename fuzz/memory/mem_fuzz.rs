#![allow(dead_code)]

// Buffers, allocation limits and reference cycles.

use std::cell::RefCell;
use std::rc::Rc;

use crate::vm::buffer::VmBuffer;
use crate::vm::gc::GcTracker;
use crate::vm::machine::VM;
use crate::vm::value::{Value, VmTable};

pub fn fuzz_buffer_integer_overflow() {
    let mut buf = VmBuffer::new(64);

    let overflow_offsets = [
        usize::MAX,
        usize::MAX - 1,
        usize::MAX - 2,
        usize::MAX - 4,
        usize::MAX - 8,
        usize::MAX / 2,
        (isize::MAX as usize) + 1,
    ];

    for &off in &overflow_offsets {
        assert!(buf.read_u8(off).is_err());
        assert!(buf.write_u8(off, 0xFF).is_err());
        for size in [1, 2, 4, 8, 10] {
            assert!(buf.range(off, size).is_err());
        }
    }
}

pub fn fuzz_buffer_copy_boundaries() {
    let buf = VmBuffer::new(16);
    assert!(buf.range(usize::MAX, 4).is_err());
    assert!(buf.range(0, usize::MAX).is_err());
    assert!(buf.range(10, 10).is_err());
    assert!(buf.range(12, 4).is_ok());
}

pub fn fuzz_gc_cyclic_stress() {
    // Stress test: allocate 100 clusters of 5-node mutually cyclic tables
    let mut gc = GcTracker::new();
    let mut root_refs = Vec::new();

    for cluster in 0..100 {
        let t1 = Rc::new(RefCell::new(VmTable::new()));
        let t2 = Rc::new(RefCell::new(VmTable::new()));
        let t3 = Rc::new(RefCell::new(VmTable::new()));
        let t4 = Rc::new(RefCell::new(VmTable::new()));
        let t5 = Rc::new(RefCell::new(VmTable::new()));

        gc.register_table(&t1);
        gc.register_table(&t2);
        gc.register_table(&t3);
        gc.register_table(&t4);
        gc.register_table(&t5);

        // Cyclic ring: t1 -> t2 -> t3 -> t4 -> t5 -> t1
        t1.borrow_mut().set_str("next", Value::table(t2.clone()));
        t2.borrow_mut().set_str("next", Value::table(t3.clone()));
        t3.borrow_mut().set_str("next", Value::table(t4.clone()));
        t4.borrow_mut().set_str("next", Value::table(t5.clone()));
        t5.borrow_mut().set_str("next", Value::table(t1.clone()));

        if cluster % 10 == 0 {
            // Keep 1 out of 10 clusters alive in roots
            root_refs.push(Value::table(t1));
        }
    }

    let empty_globals = crate::vm::hash::new_map();
    let plan = gc.collect_plan(&root_refs, &empty_globals);
    assert!(plan.queue.is_empty(), "cyclic stress uses no finalizers");
    let crate::vm::gc::SweepPlan { plain, dropped, .. } = plan;
    gc.sweep_finish(plain, dropped, Vec::new(), 0);
    let freed = gc.last_freed;
    assert!(
        freed > 0,
        "GC sweep must break unreachable cyclic table clusters"
    );
}

pub fn fuzz_vm_recursion_protection() {
    // Tests that deep NON-tail recursion is safely caught by call depth
    // guards. The `+ 1` keeps every iteration on a new frame; pure tail
    // calls (`return f()`) reuse the frame and legitimately run forever.
    // Self-recursion (not mutual) avoids forward references.
    let code = "
        local function f() return f() + 1 end
        return f()
    ";
    let stmts = crate::compiler::compile_source(code).expect("syntax error");
    let proto = crate::compiler::compile_to_proto(&stmts);
    let mut vm = VM::new();
    let res = vm.execute(proto);
    assert!(
        res.is_err(),
        "mutual infinite recursion must fail safely without stack overflow"
    );
    assert!(res.unwrap_err().contains("call stack overflow"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_buffer_integer_overflow() {
        fuzz_buffer_integer_overflow();
    }

    #[test]
    fn test_memory_buffer_copy_boundaries() {
        fuzz_buffer_copy_boundaries();
    }

    #[test]
    fn test_memory_gc_cyclic_stress() {
        fuzz_gc_cyclic_stress();
    }

    #[test]
    fn test_memory_vm_recursion_protection() {
        fuzz_vm_recursion_protection();
    }
}
