use std::path::Path;
use std::rc::Rc;

use super::decompile;
use crate::bytecode::instruction::Instruction;
use crate::bytecode::proto::Proto;
use crate::compiler::{OptLevel, Options, compile_program};
use crate::vm::VM;

const LOST: &str = "could not be recovered";
/// Whether `text` holds the call a generic `for` falls back to when its
/// source isn't one expression: `(function(...) return ... end)(f, s, c)`.
/// Other inline vararg functions are ordinary output, so the body must be
/// exactly `return ...`.
fn has_pass_through(text: &str) -> bool {
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if line.trim_end().ends_with("(function(...)")
            && lines.clone().next().map(str::trim) == Some("return ...")
        {
            return true;
        }
    }
    false
}

/// Compiles `source`, decompiles it, and checks that the decompiled source
/// compiles again and returns what the original returns. Returns the
/// decompiled source.
fn roundtrip(source: &str) -> String {
    roundtrip_with(source, Options::RUN)
}

fn roundtrip_with(source: &str, options: Options) -> String {
    let proto = compile_program(source, "original.nyk", options).expect("the original compiles");
    let expected = VM::new()
        .execute_program(proto.clone())
        .expect("the original runs");
    let text = decompile(&proto);
    assert!(
        !text.contains(LOST),
        "a function was not recovered:\n{}",
        text
    );
    let again = compile_program(&text, "decompiled.nyk", Options::UNCHECKED)
        .unwrap_or_else(|err| panic!("the decompiled source does not compile: {}\n{}", err, text));
    let got = VM::new()
        .execute_program(again)
        .unwrap_or_else(|err| panic!("the decompiled source fails: {}\n{}", err, text));
    assert!(
        got == expected,
        "the decompiled source returns something else:\n{}",
        text
    );
    text
}

#[test]
fn test_straight_line_code_and_names() {
    let src = r##"
local width = 3
local height = width * 2 + 1
local label = "area: " .. width * height
local parts = { width, height, name = "box", [10] = true }
return label .. " " .. #parts .. " " .. parts.name .. " " .. tostring(parts[10])
"##;
    let text = roundtrip(src);
    assert!(text.contains("local label = "), "{}", text);
    assert!(text.contains("local parts = "), "{}", text);
    assert!(text.contains("name = \"box\""), "{}", text);
    let text_o0 = roundtrip_with(src, Options::RUN.with_opt_level(OptLevel::O0));
    assert!(text_o0.contains("local width = 3"), "{}", text_o0);
    assert!(text_o0.contains("name = \"box\""), "{}", text_o0);
}

#[test]
fn test_idiv_and_mod_constant_ops() {
    roundtrip(
        r##"
local function calc(n)
    return n // 3, n % 5
end
local a, b = calc(17)
return a * 10 + b
"##,
    );
}

#[test]
fn test_branches_and_loops() {
    roundtrip(
        r##"
local out = {}
local function classify(n)
    if n < 0 then
        return "negative"
    elseif n == 0 then
        return "zero"
    elseif n % 2 == 0 then
        return "even"
    else
        return "odd"
    end
end
for i = -1, 6 do
    if i == 5 then
        break
    elseif i == 2 then
        continue
    end
    out[#out + 1] = classify(i)
end
local n = 0
while n < 10 do
    n += 3
end
repeat
    n -= 1
until n <= 4
for i = 10, 1, -3 do
    out[#out + 1] = tostring(i)
end
local total = 0
for k, v in { a = 1, b = 2, c = 3 } do
    total += v
end
for _, v in ipairs({ 4, 5, 6 }) do
    total += v
end
local x = nil
local y = x ?? "fallback"
local z = (total > 10 and "big") or "small"
return table.concat(out, ",") .. "|" .. n .. "|" .. total .. "|" .. y .. "|" .. z
"##,
    );
}

#[test]
fn test_nested_loops_with_early_exits() {
    roundtrip(
        r##"
local function find(grid, wanted)
    for r, row in grid do
        for c, cell in row do
            if cell == wanted then
                return r, c
            end
        end
    end
    return nil
end
local grid = { { 1, 2 }, { 3, 4 }, { 5, 6 } }
local r, c = find(grid, 4)
local hits = 0
for i = 1, 5 do
    local j = 0
    while true do
        j += 1
        if j > i then
            break
        end
        if (i + j) % 3 == 0 then
            continue
        end
        hits += 1
    end
end
return `{r},{c},{hits},{find(grid, 9)}`
"##,
    );
}

#[test]
fn test_closures_and_upvalues() {
    roundtrip(
        r##"
local function counter(start)
    local n = start
    return function(step)
        n += step ?? 1
        return n
    end
end
local next = counter(10)
next()
next(5)
local fns = {}
for i = 1, 3 do
    local doubled = i * 2
    fns[i] = function() return i + doubled end
end
local acc = {}
for _, f in fns do
    acc[#acc + 1] = f()
end
local function fib(n)
    if n < 2 then return n end
    return fib(n - 1) + fib(n - 2)
end
return next(0) .. ":" .. table.concat(acc, ",") .. ":" .. fib(15)
"##,
    );
}

#[test]
fn test_calls_methods_and_varargs() {
    let text = roundtrip(
        r##"
local Stack = {}
Stack.__index = Stack
function Stack.new()
    return setmetatable({ items = {} }, Stack)
end
function Stack:push(...)
    for _, v in { ... } do
        self.items[#self.items + 1] = v
    end
    return self
end
function Stack:size()
    return #self.items
end
local function count(...)
    return select("#", ...)
end
local function pair()
    return 1, 2
end
local s = Stack.new():push(1, 2):push(pair())
local packed = { pair(), pair() }
return s:size() .. "/" .. count(pair()) .. "/" .. count(pair(), 3) .. "/" .. #packed
"##,
    );
    assert!(text.contains(":push("), "{}", text);
}

#[test]
fn test_globals_and_strings() {
    roundtrip(
        r##"
global greeting = "hi"
local function shout(s)
    return s:upper() .. "!"
end
greeting = shout(greeting)
local quoted = "tab\tquote\" backslash\\ brace{ nul\0 end"
local bits = (0xF0 & 0x3C) | (1 << 4) ~ 2
return `{greeting} {#quoted} {bits} {-(-3)} {2 ^ 10} {7 // 2} {1.5}`
"##,
    );
}

#[test]
fn test_generic_for_sources() {
    let text = roundtrip(
        r##"
local sum = 0
for v in { 1, 2, 3, extra = 4 } do
    sum += v
end
local fields = { a = 1, b = 2 }
local keys = 0
for k in fields do
    keys += 1
end
local function step(state, i)
    i = i + 1
    if i > #state then return nil end
    return i, state[i]
end
local function each(t)
    return step, t, 0
end
local seen = {}
for i, v in each({ "p", "q" }) do
    seen[i] = v
end
local ip = {}
for i, v in ipairs({ "a", "b", nil, "d" }) do
    ip[i] = v
end
return `{sum} {keys} {table.concat(seen, ",")} {#ip}`
"##,
    );
    assert!(!has_pass_through(&text), "{}", text);
}

/// The optimizer hoists the loop body's copy of `ip` between the `ipairs`
/// call and the loop. `ip`'s register is later reused for `step`, which
/// `each` captures, but no closure exists while the call runs, so the call
/// can still become the loop's source (#106).
#[test]
fn test_generic_for_source_past_a_hoisted_copy() {
    let text = roundtrip(
        r##"
for k, v in pairs({ a = 1, b = 2, 3 }) do
end
local ip = {}
for i, v in ipairs({ "a", "b", nil, "d" }) do
    ip[i] = v
end
local count = #ip
local function step(state, i)
    i = i + 1
    if i > #state then return nil end
    return i, state[i]
end
local function each(t)
    return step, t, 0
end
local seen = 0
for _ in each({ "p", "q" }) do seen += 1 end
return `{count} {seen}`
"##,
    );
    assert!(!has_pass_through(&text), "{}", text);
    assert!(text.contains("in ipairs({"), "{}", text);
}

/// The repository's own self-checking tests still pass after a round trip
/// through the decompiler.
#[test]
fn test_repository_tests_survive_a_roundtrip() {
    let files = [
        "assignment.nyk",
        "bitwise_operators.nyk",
        "coalesce.nyk",
        "coalesce_precedence.nyk",
        "coroutines.nyk",
        "do_block.nyk",
        "enums_and_match.nyk",
        "for_loops.nyk",
        "metatable.nyk",
        "methods.nyk",
        "multiple_values.nyk",
        "repeat_until.nyk",
        "string_literals.nyk",
        "table_sort_cmp.nyk",
        "tables_and_iteration.nyk",
    ];
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    for file in files {
        let path = dir.join(file);
        let source = std::fs::read_to_string(&path).expect("the test exists");
        let name = path.to_string_lossy().to_string();
        let proto = compile_program(&source, &name, Options::RUN).expect("the test compiles");
        let text = decompile(&proto);
        assert!(
            !text.contains(LOST),
            "{}: a function was not recovered:\n{}",
            file,
            text
        );
        // Every loop in these files is an ordinary `for ... in <expr>`.
        assert!(
            !has_pass_through(&text),
            "{}: a generic for loop fell back to a pass-through call:\n{}",
            file,
            text
        );
        crate::vm::run_bytes(&name, text.clone().into_bytes(), Options::UNCHECKED)
            .unwrap_or_else(|err| panic!("{} fails after decompiling: {}\n{}", file, err, text));
    }
}

#[test]
fn test_damaged_functions_keep_their_disassembly() {
    let mut child = Proto::new(Some("broken".to_string()), 1, false);
    child.instructions = vec![Instruction::Jump { offset: 40 }];
    child.lines = vec![1];
    let mut main = Proto::new(Some("main".to_string()), 0, false);
    main.protos.push(Rc::new(child));
    main.instructions = vec![
        Instruction::Closure {
            dst: 0,
            proto_idx: 0,
        },
        Instruction::Return { base: 0, count: 1 },
    ];
    main.lines = vec![1, 1];
    let text = decompile(&main);
    assert!(text.contains(LOST), "{}", text);
    assert!(text.contains("JUMP"), "{}", text);
    assert!(text.contains("return function(arg1)"), "{}", text);
}

/// `has_pass_through` recognizes the call `lift` builds when a generic
/// `for` has no single source, wherever it is printed.
#[test]
fn test_has_pass_through_sees_the_fallback() {
    use super::ast::{Body, Expr, Func, Printer, Stmt};
    let pass = Func {
        params: Vec::new(),
        vararg: true,
        body: Body::Code(vec![Stmt::Return(vec![Expr::Vararg])]),
        names: Vec::new(),
        captures: Vec::new(),
    };
    let call = Expr::Call(
        Box::new(Expr::Function(Box::new(pass))),
        vec![Expr::Name("next".to_string()), Expr::Nil, Expr::Nil],
    );
    for depth in 0..3 {
        let text = Printer::default().expr(&call, &[], depth);
        assert!(has_pass_through(&text), "{}", text);
    }
}
