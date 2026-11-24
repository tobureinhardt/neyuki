//! The `neyuki-ext` SDK (crates/neyuki-ext) against the host side of the
//! extension ABI: an SDK extension is initialized in-process and driven
//! from scripts the way `lib/ffi.nyk` calls extension functions.

use std::cell::RefCell;
use std::ffi::c_void;
use std::time::Duration;

use neyuki_ext::{Module, Permission, Persistent, Return, Table, Value, args, tasks};

use super::ext::{NeyukiExtApi, init_extension_with_fn};
use crate::vm::machine::VM;
use crate::vm::value::{StrRef, Value as HostValue};

thread_local! {
    static KEPT: RefCell<Option<Persistent>> = const { RefCell::new(None) };
}

#[repr(C)]
struct Greeter {
    greet: extern "C" fn() -> i64,
}

extern "C" fn forty_two() -> i64 {
    42
}

static GREETER: Greeter = Greeter { greet: forty_two };

fn tick_pump(_wait: Duration) -> Result<Value, String> {
    Ok(Table::from_list(["tick"])?.into())
}

fn init(m: &mut Module) -> Result<(), String> {
    m.function("area", |a| {
        Ok(args::number_arg(a, 0, "width")? * args::number_arg(a, 1, "height")?)
    })?
    .function("shout", |a| Ok(args::str_arg(a, 0, "text")?.to_uppercase()))?
    .function("divmod", |a| {
        let (n, d) = (args::int_arg(a, 0, "n")?, args::int_arg(a, 1, "d")?);
        if d == 0 {
            return Err("division by zero".to_string());
        }
        Ok((n / d, n % d))
    })?
    .function("record", |a| {
        let t = Table::from_fields([
            ("name", Value::from(args::str_arg(a, 0, "name")?)),
            ("tags", Table::from_list(["a", "b"])?.into()),
        ])?;
        t.push(true)?;
        Ok(t)
    })?
    .function("sum_fields", |a| {
        let t = args::table_arg(a, 0, "t")?;
        let mut total = 0;
        for (_, v) in t.pairs()? {
            total += v.as_i64().unwrap_or(0);
        }
        Ok(total)
    })?
    .function("apply", |a| {
        let f = args::function_arg(a, 0, "f")?;
        f.call(&a[1..])
    })?
    .function("invert", |a| {
        let b = args::buffer_arg(a, 0, "b")?;
        b.with_bytes(|bytes| bytes.iter_mut().for_each(|x| *x = !*x))?;
        neyuki_ext::Buffer::new(&b.to_vec()?)
    })?
    .function("keep", |a| {
        let f = args::function_arg(a, 0, "f")?;
        let kept = f.retain()?;
        KEPT.with(|k| *k.borrow_mut() = Some(kept));
        Ok(())
    })?
    .function("call_kept", |_| {
        let kept = KEPT.with(|k| k.borrow().as_ref().map(|p| p.value()));
        match kept {
            Some(Ok(Value::Function(f))) => f.call(&[]),
            _ => Err("nothing kept".to_string()),
        }
    })?
    .function("drop_kept", |_| {
        KEPT.with(|k| k.borrow_mut().take());
        Ok(())
    })?
    .function("panics", |_| -> Result<(), String> { panic!("oh no") })?
    .function("can_read", |a| {
        neyuki_ext::check_permission(Permission::Read, args::str_arg(a, 0, "path")?)?;
        Ok(true)
    })?
    .function("poll", |a| {
        let secs = args::opt_number_arg(a, 0, "timeout", 0.0)?;
        if let Some(parked) = tasks::park(Duration::from_secs_f64(secs), tick_pump)? {
            return Ok(parked);
        }
        Ok(Return::from(vec![tick_pump(Duration::ZERO)?]))
    })?
    .function("due", |_| Ok(tasks::next_due().map(|d| d.as_secs_f64())))?
    .function("greet", |_| {
        let svc = unsafe { neyuki_ext::lookup_service::<Greeter>("test.greeter/1") };
        Ok(svc.map(|g| (g.greet)()))
    })?
    .provide("test.greeter/1", &GREETER)?;
    Ok(())
}

unsafe extern "C" fn sdk_init(api: *const NeyukiExtApi, ctx: *mut c_void) -> i32 {
    unsafe { neyuki_ext::__init(api.cast(), ctx, init) }
}

/// Runs `src` with the extension's functions as the global table `ext`.
fn run(src: &str) -> (VM, Result<HostValue, String>) {
    let mut vm = VM::new();
    let (id, map) = init_extension_with_fn(&mut vm, sdk_init, None).expect("SDK init");
    vm.globals
        .insert(StrRef::from("EXT"), HostValue::from_usize(id as usize));
    vm.globals.insert(
        StrRef::from("F"),
        HostValue::table(std::rc::Rc::new(RefCell::new(map))),
    );
    let prelude = "local ext = setmetatable({}, { __index = function(_, k)\n\
                   return function(...) return __ffi_ext_call(EXT, F[k], ...) end\n\
                   end })\n";
    let result = crate::compiler::compile_program(
        &format!("{prelude}{src}"),
        "sdk",
        crate::compiler::Options::UNCHECKED,
    )
    .and_then(|proto| vm.execute_program(proto));
    (vm, result)
}

fn eval(src: &str) -> String {
    match run(src).1 {
        Ok(v) => v.to_string(),
        Err(e) => panic!("script failed: {e}"),
    }
}

fn error(src: &str) -> String {
    run(src).1.expect_err("script should fail")
}

#[test]
fn scalars_strings_and_multiple_returns() {
    assert_eq!(eval("return ext.area(3, 2.5)"), "7.5");
    assert_eq!(eval("return ext.shout('héllo')"), "HÉLLO");
    assert_eq!(
        eval("local q, r = ext.divmod(17, 5) return q .. ',' .. r"),
        "3,2"
    );
}

#[test]
fn errors_use_the_runtime_wording_and_panics_are_caught() {
    assert!(error("return ext.area('x', 1)").contains("width must be a number, got string"));
    assert!(error("return ext.area()").contains("width must be provided"));
    assert!(error("return ext.divmod(1, 0)").contains("division by zero"));
    let caught = eval(
        "local ok, err = pcall(ext.panics)\n\
         return tostring(ok) .. ' ' .. tostring(err)",
    );
    assert!(
        caught.contains("false") && caught.contains("panicked: oh no"),
        "{caught}"
    );
}

#[test]
fn tables_in_and_out() {
    assert_eq!(
        eval(
            "local r = ext.record('yuki') return r.name .. #r.tags .. r.tags[2] .. tostring(r[1])"
        ),
        "yuki2btrue"
    );
    assert_eq!(eval("return ext.sum_fields({ a = 1, b = 2, 3 })"), "6");
}

#[test]
fn callbacks_buffers_and_persistent_handles() {
    assert_eq!(
        eval("return ext.apply(function(a, b) return a + b end, 2, 3)"),
        "5"
    );
    assert!(error("return ext.apply(function() error('inner') end)").contains("inner"));
    assert_eq!(
        eval(
            "local b = buffer.fromstring('AB') local c = ext.invert(b)\n\
              return buffer.readu8(b, 0) .. ',' .. buffer.readu8(c, 0)"
        ),
        "190,190"
    );
    assert_eq!(
        eval("ext.keep(function() return 'kept' end) collectgarbage() return ext.call_kept()"),
        "kept"
    );
    assert!(
        error("ext.keep(print) ext.drop_kept() return ext.call_kept()").contains("nothing kept")
    );
}

#[test]
fn permissions_follow_the_sandbox() {
    assert_eq!(eval("return ext.can_read('/etc/hosts')"), "true");
    let mut vm = VM::new();
    let (id, map) = init_extension_with_fn(&mut vm, sdk_init, None).unwrap();
    vm.set_permissions(crate::vm::permissions::Permissions::deny_all());
    let idx = map.get_str("can_read").as_int().expect("integer index") as usize;
    let err = super::ext::call_ext(&mut vm, id, idx, &[HostValue::str("/etc/hosts")]).unwrap_err();
    assert!(err.contains("permission denied"), "{err}");
}

#[test]
fn tasks_park_on_the_pump_and_services_are_shared() {
    let (vm, result) = run("task.spawn(function() _G.first = ext.poll(5)[1] end)\n\
         _G.before = _G.first\n\
         _G.inline = ext.poll()[1]\n\
         _G.idle = ext.due()\n\
         _G.greet = ext.greet()");
    result.expect("program runs");
    let global = |n: &str| {
        vm.globals
            .get(&StrRef::from(n))
            .cloned()
            .unwrap_or(HostValue::NIL)
    };
    assert!(global("before").is_nil());
    assert_eq!(global("first").to_string(), "tick");
    assert_eq!(global("inline").to_string(), "tick");
    assert_eq!(global("greet").to_string(), "42");
}
