//! The `table` library.

use crate::vm::libs::args::{frozen, int_arg, opt_int_arg, table_arg};
use crate::vm::machine::VM;
use crate::vm::value::{BorrowedTable, TableBorrow, Value, ValueView, VmTable};

const MAX_CREATE: i64 = 100_000_000;

fn writable<'a>(args: &'a [Value], name: &str) -> Result<BorrowedTable<'a>, String> {
    let t = table_arg(args, 0, name)?;
    if t.borrow().is_frozen() {
        return Err("attempted to mutate a frozen table".to_string());
    }
    Ok(t)
}

fn new_table(vm: &mut VM, table: VmTable) -> Value {
    vm.alloc_table(table)
}

/// Every key and value of a table in iteration order: the array part, then
/// string keys sorted, then the remaining keys.
pub(crate) fn entries(table: &VmTable) -> Vec<(Value, Value)> {
    let mut out = Vec::with_capacity(table.array.len() + table.fields.len());
    for (i, v) in table.array.iter().enumerate() {
        if !v.is_nil() {
            out.push((Value::from_usize(i + 1), v.clone()));
        }
    }
    for key in crate::vm::table::sorted_keys(table) {
        let value = crate::vm::table::raw_get(table, &key);
        if !value.is_nil() {
            out.push((key, value));
        }
    }
    out
}

/// Beat/Old-unified version of [`entries`].
pub(crate) fn entries_borrow(table: &TableBorrow<'_>) -> Vec<(Value, Value)> {
    let arr = table.array_slice();
    let mut out = Vec::with_capacity(arr.len() + table.fields_len());
    for (i, v) in arr.iter().enumerate() {
        if !v.is_nil() {
            out.push((Value::from_usize(i + 1), v.clone()));
        }
    }
    for key in table.sorted_keys() {
        let value = table.raw_get(&key);
        if !value.is_nil() {
            out.push((key, value));
        }
    }
    out
}

/// Number of non-nil entries, including named keys (`#t` only counts the
/// array part).
fn table_length(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let t_handle = table_arg(args, 0, "t")?;
    let t = t_handle.borrow();
    let mut n = t.array_slice().iter().filter(|v| !v.is_nil()).count();
    t.for_each_field(|_, v| {
        if !v.is_nil() {
            n += 1;
        }
    });
    n += t.general_len();
    Ok(vec![Value::from_usize(n)])
}

/// `insert(t, v)` appends; `insert(t, pos, v)` shifts later elements up.
fn table_insert(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let _ = writable(args, "t")?;
    let tbl_val = &args[0];
    if let Some(bt_ptr) = tbl_val.beat_table_ptr() {
        match args.len() {
            0 | 1 => return Err("insert expects a value".to_string()),
            2 => {
                let len = unsafe { &*bt_ptr }.array_len();
                vm.table_raw_set(tbl_val, Value::from_usize(len + 1), args[1].clone())?;
                return Ok(Vec::new());
            }
            _ => {
                let pos = int_arg(args, 1, "pos")?;
                let len = unsafe { &*bt_ptr }.array_len() as i64;
                if pos < 1 || pos > len + 1 {
                    return Err(format!(
                        "position {} is out of bounds (1 to {})",
                        pos,
                        len + 1
                    ));
                }
                unsafe {
                    vm.heap
                        .beat
                        .table_insert_at(bt_ptr, (pos - 1) as usize, args[2].clone())?;
                }
                return Ok(Vec::new());
            }
        }
    }
    let Some(t) = tbl_val.as_table() else {
        return Err("t must be a table".to_string());
    };
    let tbl_copy = tbl_val.clone();
    let mut val = match args.len() {
        0 | 1 => return Err("insert expects a value".to_string()),
        2 => args[1].clone(),
        _ => args[2].clone(),
    };
    vm.container_write_barrier(&tbl_copy, None, &mut val);
    let mut tbl = t.borrow_mut();
    match args.len() {
        2 => {
            tbl.array.push(val);
            Ok(Vec::new())
        }
        _ => {
            let len = tbl.array.len() as i64;
            let pos = int_arg(args, 1, "pos")?;
            if pos < 1 || pos > len + 1 {
                return Err(format!(
                    "position {} is out of bounds (1 to {})",
                    pos,
                    len + 1
                ));
            }
            tbl.array.insert((pos - 1) as usize, val);
            Ok(Vec::new())
        }
    }
}

/// Removes and returns the element at `pos` (the last one by default),
/// shifting later elements down.
fn table_remove(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let _ = writable(args, "t")?;
    let tbl_val = &args[0];
    if let Some(bt_ptr) = tbl_val.beat_table_ptr() {
        let len = unsafe { &*bt_ptr }.array_len() as i64;
        let pos = opt_int_arg(args, 1, "pos", len)?;
        if len == 0 && (pos == 0 || pos == len) {
            return Ok(vec![Value::NIL]);
        }
        if pos < 1 || pos > len {
            return Err(format!("position {} is out of bounds (1 to {})", pos, len));
        }
        let bt = unsafe { &mut *bt_ptr };
        if bt.extra.is_null() && pos == len {
            let idx = (len - 1) as usize;
            let removed = unsafe {
                let slot = bt.arr_ptr.add(idx);
                std::mem::replace(&mut *slot, Value::NIL)
            };
            bt.arr_len -= 1;
            return Ok(vec![removed]);
        }
        let ext = vm.heap.beat.ensure_extra(bt);
        return Ok(vec![ext.array.remove((pos - 1) as usize)]);
    }
    let t = tbl_val.as_table().unwrap();
    let mut tbl = t.borrow_mut();
    let len = tbl.array.len() as i64;
    let pos = opt_int_arg(args, 1, "pos", len)?;
    if len == 0 && (pos == 0 || pos == len) {
        return Ok(vec![Value::NIL]);
    }
    if pos < 1 || pos > len {
        return Err(format!("position {} is out of bounds (1 to {})", pos, len));
    }
    Ok(vec![tbl.array.remove((pos - 1) as usize)])
}

fn range(args: &[Value], first: usize, len: usize) -> Result<(usize, usize), String> {
    let i = opt_int_arg(args, first, "i", 1)?;
    let j = opt_int_arg(args, first + 1, "j", len as i64)?;
    let i = i.max(1) as usize;
    let j = j.min(len as i64);
    if j < i as i64 {
        return Ok((1, 0));
    }
    Ok((i, j as usize))
}

/// Joins `t[i..j]` with `sep`. Strings and numbers are joined as they print;
/// anything else is an error, as in Lua.
fn table_concat(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let t_handle = table_arg(args, 0, "t")?;
    let t = t_handle.borrow();
    let sep = match args.get(1).map(Value::view) {
        None | Some(ValueView::Nil) => String::new(),
        Some(ValueView::String(s)) => s.to_string(),
        Some(ValueView::Int(_) | ValueView::BigInt(_) | ValueView::Float(_)) => args[1].to_string(),
        Some(_) => {
            return Err(format!("sep must be a string, got {}", args[1].type_name()));
        }
    };
    let arr = t.array_slice();
    let (i, j) = range(args, 2, arr.len())?;
    let mut out = String::new();
    for idx in i..=j {
        if idx > i {
            out.push_str(&sep);
        }
        let item = &arr[idx - 1];
        match item.view() {
            ValueView::String(s) => out.push_str(&s),
            ValueView::Int(_) | ValueView::BigInt(_) | ValueView::Float(_) => {
                out.push_str(&item.to_string())
            }
            _ => {
                return Err(format!(
                    "invalid value (a {}) at index {} in table for 'concat'",
                    item.type_name(),
                    idx
                ));
            }
        }
    }
    Ok(vec![Value::string(out)])
}

/// `create(n, v?)`: an array of `n` copies of `v`.
fn table_create(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let n = int_arg(args, 0, "n")?;
    if n < 0 {
        return Err("n must be non-negative".to_string());
    }
    if n > MAX_CREATE {
        return Err(format!("n exceeds the maximum ({})", MAX_CREATE));
    }
    let mut value = args.get(1).cloned().unwrap_or(Value::NIL);
    vm.beat_escape_if_needed(&mut value);
    let mut table = VmTable::new();
    if !value.is_nil() {
        table.array = vec![value; n as usize];
    }
    Ok(vec![new_table(vm, table)])
}

/// `find(t, v, init?)`: the first key in the array part holding `v`,
/// searching from `init`, then any other key holding it.
fn table_find(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let t_handle = table_arg(args, 0, "t")?;
    let t = t_handle.borrow();
    let needle = args.get(1).cloned().unwrap_or(Value::NIL);
    if needle.is_nil() {
        return Err("value must be provided".to_string());
    }
    let init = opt_int_arg(args, 2, "init", 1)?.max(1) as usize;
    for (i, v) in t.array_slice().iter().enumerate().skip(init - 1) {
        if *v == needle {
            return Ok(vec![Value::from_usize(i + 1)]);
        }
    }
    if init == 1 {
        for key in t.sorted_keys() {
            if t.raw_get(&key) == needle {
                return Ok(vec![key]);
            }
        }
    }
    Ok(vec![Value::NIL])
}

fn table_keys(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let mut keys = {
        let t_handle = table_arg(args, 0, "t")?;
        let t = t_handle.borrow();
        entries_borrow(&t)
            .into_iter()
            .map(|(k, _)| k)
            .collect::<Vec<_>>()
    };
    for k in &mut keys {
        vm.beat_escape_if_needed(k);
    }
    let mut table = VmTable::new();
    table.array = keys;
    Ok(vec![new_table(vm, table)])
}

fn table_values(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let mut values = {
        let t_handle = table_arg(args, 0, "t")?;
        let t = t_handle.borrow();
        entries_borrow(&t)
            .into_iter()
            .map(|(_, v)| v)
            .collect::<Vec<_>>()
    };
    for v in &mut values {
        vm.beat_escape_if_needed(v);
    }
    let mut table = VmTable::new();
    table.array = values;
    Ok(vec![new_table(vm, table)])
}

fn table_pack(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let mut tbl = VmTable::new();
    let mut arr = args.to_vec();
    for v in &mut arr {
        vm.beat_escape_if_needed(v);
    }
    tbl.array = arr;
    tbl.set_str("n", Value::from_usize(args.len()));
    Ok(vec![new_table(vm, tbl)])
}

const MAX_UNPACK: usize = 8000;

fn table_unpack(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let t_handle = table_arg(args, 0, "t")?;
    let t = t_handle.borrow();
    let arr = t.array_slice();
    let (i, j) = range(args, 1, arr.len())?;
    if j < i {
        return Ok(Vec::new());
    }
    if j - i + 1 > MAX_UNPACK {
        return Err(format!(
            "too many results to unpack (maximum {})",
            MAX_UNPACK
        ));
    }
    Ok(arr[i - 1..j].to_vec())
}

fn table_freeze(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let t = table_arg(args, 0, "t")?;
    if let Some(old) = t.as_old() {
        old.borrow_mut().frozen = true;
    } else if let Some(bt) = args[0].beat_table_ptr() {
        unsafe { (*bt).set_frozen(true) };
    }
    Ok(vec![args[0].clone()])
}

fn table_isfrozen(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![Value::bool(
        table_arg(args, 0, "t")?.borrow().is_frozen(),
    )])
}

fn table_clear(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let t = writable(args, "t")?;
    if let Some(old) = t.as_old() {
        let mut tbl = old.borrow_mut();
        tbl.array.clear();
        tbl.fields.clear();
        tbl.general = None;
    } else if let Some(bt_ptr) = args[0].beat_table_ptr() {
        unsafe {
            let bt = &mut *bt_ptr;
            if let Some(ext) = bt.extra_mut() {
                ext.array.clear();
                ext.fields.clear();
                ext.general = None;
            } else {
                bt.drop_inline_old_refs();
            }
        }
    }
    Ok(Vec::new())
}

/// A shallow copy. The metatable is shared; the copy is never frozen.
fn table_clone(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let mut copy = {
        let tbl_handle = table_arg(args, 0, "t")?;
        match tbl_handle.borrow() {
            TableBorrow::Old(tbl) => {
                let mut copy = VmTable::new();
                copy.array = tbl.array.clone();
                copy.fields = tbl.fields.clone();
                copy.general = tbl.general.clone();
                copy.metatable = tbl.metatable.clone();
                copy.co_state = tbl.co_state.clone();
                copy
            }
            TableBorrow::Beat(bt) => {
                let mut copy = bt.to_vm_table_cloned();
                copy.frozen = false;
                copy
            }
        }
    };
    for v in &mut copy.array {
        vm.beat_escape_if_needed(v);
    }
    for v in copy.fields.values_mut() {
        vm.beat_escape_if_needed(v);
    }
    Ok(vec![new_table(vm, copy)])
}

/// Stable bottom-up merge sort with a fallible "a sorts before b" test, so
/// comparator errors propagate and no recursion depth is involved.
fn merge_sort(
    items: &mut Vec<Value>,
    mut before: impl FnMut(&Value, &Value) -> Result<bool, String>,
) -> Result<(), String> {
    let n = items.len();
    if n < 2 {
        return Ok(());
    }
    let mut scratch: Vec<Value> = Vec::with_capacity(n);
    let mut width = 1;
    while width < n {
        scratch.clear();
        let mut start = 0;
        while start < n {
            let mid = (start + width).min(n);
            let end = (start + 2 * width).min(n);
            let (mut left, mut right) = (start, mid);
            while left < mid && right < end {
                if before(&items[right], &items[left])? {
                    scratch.push(items[right].clone());
                    right += 1;
                } else {
                    scratch.push(items[left].clone());
                    left += 1;
                }
            }
            scratch.extend_from_slice(&items[left..mid]);
            scratch.extend_from_slice(&items[right..end]);
            start = end;
        }
        std::mem::swap(items, &mut scratch);
        width *= 2;
    }
    Ok(())
}

/// Sorts the array part in place, by `<` or by `cmp(a, b)` returning true
/// when `a` must come first. The sort is stable.
fn table_sort(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let tbl_handle = writable(args, "t")?;
    if let Some(old) = tbl_handle.as_old() {
        let t = old.to_rc();
        let mut items = std::mem::take(&mut t.borrow_mut().array);
        let result = match args.get(1) {
            None => sort_default(&mut items),
            Some(v) if v.is_nil() => sort_default(&mut items),
            Some(cmp) => {
                let cmp = cmp.clone();
                merge_sort(&mut items, |a, b| {
                    Ok(vm
                        .call_function(cmp.clone(), &[a.clone(), b.clone()])?
                        .first()
                        .is_some_and(Value::is_truthy))
                })
            }
        };
        t.borrow_mut().array = items;
        return result.map(|_| Vec::new());
    }
    let bt_ptr = args[0].beat_table_ptr().unwrap();
    let mut items = unsafe {
        let bt = &mut *bt_ptr;
        if !bt.extra.is_null() {
            std::mem::take(&mut (*bt.extra).table.array)
        } else {
            let len = bt.arr_len as usize;
            let slice = std::slice::from_raw_parts_mut(bt.arr_ptr, len);
            let mut out = Vec::with_capacity(len);
            for slot in slice {
                out.push(std::mem::replace(slot, Value::NIL));
            }
            bt.arr_len = 0;
            out
        }
    };
    let result = match args.get(1) {
        None => sort_default(&mut items),
        Some(v) if v.is_nil() => sort_default(&mut items),
        Some(cmp) => {
            let cmp = cmp.clone();
            merge_sort(&mut items, |a, b| {
                Ok(vm
                    .call_function(cmp.clone(), &[a.clone(), b.clone()])?
                    .first()
                    .is_some_and(Value::is_truthy))
            })
        }
    };
    unsafe {
        let bt = &mut *bt_ptr;
        if !bt.extra.is_null() {
            (*bt.extra).table.array = items;
        } else {
            debug_assert!(items.len() <= bt.arr_cap as usize);
            bt.arr_len = items.len() as u32;
            for (idx, val) in items.into_iter().enumerate() {
                std::ptr::write(bt.arr_ptr.add(idx), val);
            }
        }
    }
    result.map(|_| Vec::new())
}

fn sort_default(items: &mut Vec<Value>) -> Result<(), String> {
    if items.iter().all(Value::is_int) {
        items.sort_unstable_by_key(|v| v.as_int().unwrap_or(0));
        return Ok(());
    }
    if items.iter().all(Value::is_string) {
        items.sort_unstable_by(|a, b| a.as_str().cmp(&b.as_str()));
        return Ok(());
    }
    merge_sort(items, |a, b| match (a.view(), b.view()) {
        (ValueView::Int(x), ValueView::Int(y)) => Ok(x < y),
        (ValueView::String(x), ValueView::String(y)) => Ok(x.as_str() < y.as_str()),
        _ => crate::vm::ops::eval_lt(a, b)
            .map_err(|_| format!("cannot compare {} with {}", a.type_name(), b.type_name())),
    })
}

/// `move(a1, f, e, t, a2?)`: copies `a1[f..e]` to `a2[t..]` (or within
/// `a1`), handling overlap, and returns the destination.
fn table_move(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let src_ptr = table_arg(args, 0, "a1")?.as_ptr_id();
    let f = int_arg(args, 1, "f")?;
    let e = int_arg(args, 2, "e")?;
    let t = int_arg(args, 3, "t")?;
    let (target, dest_ptr) = match args.get(4) {
        None => (args[0].clone(), src_ptr),
        Some(v) if v.is_nil() => (args[0].clone(), src_ptr),
        _ => {
            let p = table_arg(args, 4, "a2")?.as_ptr_id();
            (args[4].clone(), p)
        }
    };
    if e >= f {
        if f < 1 && e >= i64::MAX + f {
            return Err("too many elements to move".to_string());
        }
        let count = e - f + 1;
        if t > i64::MAX - count + 1 {
            return Err("destination wrap around".to_string());
        }
        let overlapping = src_ptr == dest_ptr && t > f && t <= e;
        let source = args[0].clone();
        let order: Box<dyn Iterator<Item = i64>> = if overlapping {
            Box::new((0..count).rev())
        } else {
            Box::new(0..count)
        };
        for k in order {
            let v = vm.table_get(&source, &Value::int(f + k))?;
            vm.table_set(&target, Value::int(t + k), v)?;
        }
    }
    Ok(vec![target])
}

pub fn create_table_lib() -> Value {
    let mut table = VmTable::new();
    macro_rules! reg {
        ($($name:literal => $func:ident),* $(,)?) => {
            $(table.set_str($name, crate::native!(concat!("table.", $name), $func));)*
        };
    }
    reg! {
        "clear" => table_clear,
        "clone" => table_clone,
        "concat" => table_concat,
        "create" => table_create,
        "find" => table_find,
        "freeze" => table_freeze,
        "insert" => table_insert,
        "isfrozen" => table_isfrozen,
        "keys" => table_keys,
        "length" => table_length,
        "move" => table_move,
        "pack" => table_pack,
        "remove" => table_remove,
        "sort" => table_sort,
        "unpack" => table_unpack,
        "values" => table_values,
    }
    frozen(table)
}
