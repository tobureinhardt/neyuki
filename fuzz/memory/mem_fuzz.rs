#![allow(dead_code)]

// Buffers, allocation limits and reference cycles.

use crate::vm::buffer::VmBuffer;
use crate::vm::hash::FxHashSet;
use crate::vm::machine::VM;
use crate::vm::old::OldArena;
use crate::vm::value::Value;

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
    let mut old = OldArena::new();
    let mut pins = FxHashSet::default();

    for cluster in 0..100 {
        let (id1, t1) = old.alloc_with_capacity(0, 1, 0);
        let (id2, t2) = old.alloc_with_capacity(0, 1, 0);
        let (id3, t3) = old.alloc_with_capacity(0, 1, 0);
        let (id4, t4) = old.alloc_with_capacity(0, 1, 0);
        let (id5, t5) = old.alloc_with_capacity(0, 1, 0);

        // Cyclic ring: t1 -> t2 -> t3 -> t4 -> t5 -> t1
        t1.borrow_mut().set_str("next", Value::table(t2.clone()));
        old.on_old_store(Some(id1), id2);
        t2.borrow_mut().set_str("next", Value::table(t3.clone()));
        old.on_old_store(Some(id2), id3);
        t3.borrow_mut().set_str("next", Value::table(t4.clone()));
        old.on_old_store(Some(id3), id4);
        t4.borrow_mut().set_str("next", Value::table(t5.clone()));
        old.on_old_store(Some(id4), id5);
        t5.borrow_mut().set_str("next", Value::table(t1.clone()));
        old.on_old_store(Some(id5), id1);

        if cluster % 10 == 0 {
            // Keep 1 out of 10 clusters alive in roots
            pins.insert(id1.slot);
        }
    }

    let plan = old.collect_cycles(&pins);
    assert!(
        plan.finalizers.is_empty(),
        "cyclic stress uses no finalizers"
    );
    assert!(
        plan.bytes_freed > 0,
        "OldArena cycle collector must break unreachable cyclic table clusters"
    );
    assert_eq!(old.live_count(), 50);
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
