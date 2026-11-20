//! Call frames.

use crate::vm::value::{Value, VmClosure};
use std::rc::Rc;

pub struct CallFrame {
    pub closure: Rc<VmClosure>,
    pub ip: usize,
    // Base index into the VM register stack for this frame
    pub base: usize,
    // Boxed slice avoids carrying `Vec`'s capacity word on every non-vararg frame.
    varargs: Option<Box<[Value]>>,
    // How many values the caller's `Call` asked for; MULTRET means "all of them"
    pub want_ret: u8,
    /// The line and instruction a line hook last reported in this frame.
    pub hook_line: u32,
    pub hook_ip: usize,
}

impl CallFrame {
    #[inline]
    pub fn new(closure: Rc<VmClosure>, base: usize) -> Self {
        Self {
            closure,
            ip: 0,
            base,
            varargs: None,
            want_ret: 1,
            hook_line: 0,
            hook_ip: 0,
        }
    }

    #[inline]
    pub fn with_varargs(closure: Rc<VmClosure>, base: usize, varargs: Vec<Value>) -> Self {
        Self {
            closure,
            ip: 0,
            base,
            varargs: if varargs.is_empty() {
                None
            } else {
                Some(varargs.into_boxed_slice())
            },
            want_ret: 1,
            hook_line: 0,
            hook_ip: 0,
        }
    }

    #[inline]
    pub fn varargs(&self) -> &[Value] {
        self.varargs.as_deref().unwrap_or(&[])
    }

    #[inline]
    pub fn varargs_mut(&mut self) -> &mut [Value] {
        self.varargs.as_deref_mut().unwrap_or(&mut [])
    }

    #[inline]
    pub fn wanting(mut self, want_ret: u8) -> Self {
        self.want_ret = want_ret;
        self
    }
}

const _: () = assert!(std::mem::size_of::<CallFrame>() <= 56);
