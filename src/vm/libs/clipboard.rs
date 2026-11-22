//! The system clipboard behind `ui.clipboard`, through `arboard`.
//!
//! `@nyk/hid` reaches the same clipboard with its own copy of this code.

use std::cell::RefCell;

use arboard::Clipboard;

use crate::vm::libs::args::str_arg;
use crate::vm::machine::VM;
use crate::vm::value::{NativeDef, Value};

pub(crate) const NATIVES: &[NativeDef] = &[
    NativeDef {
        name: "__ui_clipboard_get",
        func: builtin_clipboard_get,
    },
    NativeDef {
        name: "__ui_clipboard_set",
        func: builtin_clipboard_set,
    },
];

thread_local! {
    static CLIPBOARD: RefCell<Option<Clipboard>> = const { RefCell::new(None) };
}

fn with_clipboard<T>(f: impl FnOnce(&mut Clipboard) -> Result<T, String>) -> Result<T, String> {
    CLIPBOARD.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let clipboard =
                Clipboard::new().map_err(|err| format!("cannot open the clipboard: {}", err))?;
            *slot = Some(clipboard);
        }
        f(slot.as_mut().expect("opened above"))
    })
}

/// `()`: the clipboard text, or nil when it holds none.
fn builtin_clipboard_get(_vm: &mut VM, _args: &[Value]) -> Result<Vec<Value>, String> {
    with_clipboard(|clipboard| match clipboard.get_text() {
        Ok(text) => Ok(vec![Value::string(text)]),
        Err(arboard::Error::ContentNotAvailable) => Ok(vec![Value::Nil]),
        Err(err) => Err(format!("cannot read the clipboard: {}", err)),
    })
}

/// `(text)`: replaces the clipboard contents with text.
fn builtin_clipboard_set(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let text = str_arg(args, 0, "text")?;
    with_clipboard(|clipboard| {
        clipboard
            .set_text(text)
            .map_err(|err| format!("cannot write the clipboard: {}", err))
    })?;
    Ok(vec![])
}
