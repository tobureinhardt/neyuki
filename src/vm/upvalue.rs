//! Open upvalue management, capture, closing, and coroutine parking.

use std::rc::Rc;

use crate::vm::machine::VM;
use crate::vm::value::{Upvalue, Value};

/// An open upvalue parked with a suspended stack: the register index it will
/// reopen at, and the upvalue itself (holding the value while parked).
pub type ParkedUpvalue = (usize, Rc<Upvalue>);

impl VM {
    /// Moves every open upvalue at or above `from` off the stack and into
    /// its own cell. Called when the frame owning those registers ends.
    pub fn close_upvalues(&mut self, from: usize) {
        while let Some((idx, _)) = self.open_upvalues.last() {
            if *idx < from {
                break;
            }
            let (idx, uv) = self
                .open_upvalues
                .pop()
                .expect("the loop condition saw an open upvalue");
            let val = self.stack.get(idx).cloned().unwrap_or(Value::Nil);
            uv.close(val);
        }
    }

    /// Closes the open upvalue for stack slot `idx`, if there is one, so the
    /// next capture of that slot gets a fresh variable.
    pub(crate) fn close_upvalue_at(&mut self, idx: usize) {
        let pos = self.open_upvalues.partition_point(|(i, _)| *i < idx);
        if self.open_upvalues.get(pos).is_some_and(|(i, _)| *i == idx) {
            let (_, uv) = self.open_upvalues.remove(pos);
            let val = self.stack.get(idx).cloned().unwrap_or(Value::Nil);
            uv.close(val);
        }
    }

    /// Finds or creates the open upvalue for stack slot `idx`.
    pub(crate) fn capture_upvalue(&mut self, idx: usize) -> Rc<Upvalue> {
        if idx >= self.stack.len() {
            self.stack.resize(idx + 1, Value::Nil);
        }
        let pos = self.open_upvalues.partition_point(|(i, _)| *i < idx);
        if let Some((i, uv)) = self.open_upvalues.get(pos)
            && *i == idx
        {
            return uv.clone();
        }
        let uv = Upvalue::open(idx);
        self.open_upvalues.insert(pos, (idx, uv.clone()));
        uv
    }

    /// Detaches every open upvalue from the current stack so the stack can be
    /// put aside (a coroutine switch). Each keeps its value in its cell until
    /// `unpark_upvalues` reattaches it.
    pub fn park_upvalues(&mut self) -> Vec<ParkedUpvalue> {
        let parked = std::mem::take(&mut self.open_upvalues);
        for (idx, uv) in &parked {
            let val = self.stack.get(*idx).cloned().unwrap_or(Value::Nil);
            uv.close(val);
        }
        parked
    }

    /// Reattaches upvalues parked by `park_upvalues` to the (now current)
    /// stack they belong to, copying any writes made meanwhile back into
    /// their registers.
    pub fn unpark_upvalues(&mut self, parked: Vec<ParkedUpvalue>) {
        for (idx, uv) in &parked {
            let val = std::mem::replace(&mut *uv.closed.borrow_mut(), Value::Nil);
            self.set_reg_grow(*idx, val);
            uv.location.set(Some(*idx));
        }
        self.open_upvalues = parked;
    }

    #[inline]
    pub fn upvalue_get(&self, uv: &Upvalue) -> Value {
        match uv.location.get() {
            Some(idx) => self.stack.get(idx).cloned().unwrap_or(Value::Nil),
            None => uv.closed.borrow().clone(),
        }
    }

    #[inline]
    pub fn upvalue_set(&mut self, uv: &Upvalue, val: Value) {
        match uv.location.get() {
            Some(idx) => self.set_reg_grow(idx, val),
            None => *uv.closed.borrow_mut() = val,
        }
    }
}
