# Stage 1: VmHeap Handle & Allocation Abstraction Implementation Plan

> **For agentic workers:** Execute task-by-task cleanly, keeping all tests green after each task and verifying with `cargo fmt`, `cargo clippy`, and `cargo test` before committing.

**Goal:** Isolate every VM heap allocation (`VmTable`, `VmBuffer`, `VmClosure`), closure/table extraction, and container mutation across `src/vm/` behind `VmHeap` / `VM` methods while preserving the existing `Rc` + `GcTracker` runtime behavior underneath.

**Architecture:** Introduce `src/vm/heap.rs` (`pub struct VmHeap`) with allocation, borrowing, and table-write choke-point methods exposed on `VM` (`vm.alloc_table`, `vm.alloc_frozen`, `vm.alloc_array`, `vm.alloc_buffer`, `vm.alloc_closure`, `vm.table_raw_get`, `vm.table_raw_set`, `vm.table_set_str`, `vm.table_set_metatable`). Migrate `libs/args.rs`, `builtins/*`, `libs/*`, `call.rs`, `ops.rs`, `machine.rs`, `interp.rs`, and `threaded.rs` away from direct `Rc::new(RefCell::new(...))` / `Value::table(...)` / `Value::buffer(...)` / `Value::closure(...)` calls.

**Tech Stack:** Rust (Edition 2024), existing 64-bit NaN-boxed `Value(u64)` (`src/vm/value.rs`), `GcTracker` (`src/vm/gc.rs`).

**Spec:** `docs/superpowers/specs/2026-11-24-memory-follows-work-design.md` (Section 5, Stage 1).

## Global Constraints

- Keep 64-bit Negative-QNaN `Value(u64)` layout intact (`src/vm/value.rs`).
- Preserve 100% of existing behavior and pass all 599 lib tests, 21 bin tests, 4 `neyuki-ext` tests, and strict `cargo clippy -- -D warnings`.
- Zero `#[allow(...)]` suppressions and zero stub/placeholder functions.
- `threaded.rs` fast paths must keep zero tail-call regressions.

---

### Task 1: Introduce `src/vm/heap.rs` (`VmHeap`) and `VM` Heap API

**Files:**
- Create: `src/vm/heap.rs`
- Modify: `src/vm/mod.rs:1-20`
- Modify: `src/vm/machine.rs` (`VM` struct + `VM::new_empty`)

**Interfaces:**
- Produces on `crate::vm::heap::VmHeap` and `impl VM`:
  - `pub fn alloc_table(&mut self, table: VmTable) -> Value`
  - `pub fn alloc_table_cap(&mut self, array: usize, fields: usize) -> Value`
  - `pub fn alloc_array(&mut self, values: Vec<Value>) -> Value`
  - `pub fn alloc_frozen(&mut self, table: VmTable) -> Value`
  - `pub fn alloc_buffer(&mut self, buf: VmBuffer) -> Value`
  - `pub fn alloc_closure(&mut self, proto: Rc<Proto>, upvalues: Vec<Rc<Upvalue>>) -> Value`
  - `pub fn table_set_str(&mut self, table: &Value, key: &str, val: Value)`
  - `pub fn table_raw_set(&mut self, table: &Value, key: Value, val: Value) -> Result<(), &'static str>`
  - `pub fn table_set_metatable(&mut self, table: &Value, mt: Option<Value>) -> bool`

- [ ] **Step 1: Write unit tests in `src/vm/heap.rs` covering allocation, `alloc_frozen`, `alloc_array`, `table_raw_set`, `table_set_str`, and `table_set_metatable`.**
- [ ] **Step 2: Create `src/vm/heap.rs`, declare `pub mod heap;` in `src/vm/mod.rs`, add `pub heap: VmHeap` (or delegate to `self.gc`) on `VM` in `src/vm/machine.rs`.**
- [ ] **Step 3: Run `cargo test --lib vm::heap` and verify PASS.**
- [ ] **Step 4: Commit Task 1.**

---

### Task 2: Migrate `src/vm/libs/args.rs` and Simple Table-Creating Libraries

**Files:**
- Modify: `src/vm/libs/args.rs`, `src/vm/libs/bit.rs`, `src/vm/libs/buffer.rs`, `src/vm/libs/csv.rs`, `src/vm/libs/json.rs`, `src/vm/libs/log.rs`, `src/vm/libs/os.rs`, `src/vm/libs/strlib.rs`, `src/vm/libs/table.rs`, `src/vm/libs/toml.rs`, `src/vm/libs/utf8.rs`, `src/vm/libs/yaml.rs`

**Interfaces:**
- Consumes `VM::alloc_table`, `VM::alloc_frozen`, `VM::alloc_array`, `VM::alloc_buffer` (and keeps `BuiltinTemplate::build()` passing `&mut scratch` cleanly for `create_*_lib(vm)` or `VmHeap::frozen_static`).

- [ ] **Step 1: Update `args::array(vm, values)` and `args::frozen(vm, table)` (keep `frozen_static(table)` for `create_*_lib()` static library tables if no `&mut VM` is passed yet, or pass `&mut VM` from `BuiltinTemplate::build()`).**
- [ ] **Step 2: Replace direct `Rc::new(RefCell::new(...))` / `Value::table(...)` / `Value::buffer(...)` in the libraries above with `vm.alloc_*` calls.**
- [ ] **Step 3: Run `cargo test --lib` to verify all stdlib & integration tests pass.**
- [ ] **Step 4: Commit Task 2.**

---

### Task 3: Migrate `builtins/*`, `coroutine.rs`, `debug.rs`, `http.rs`, `task.rs`, and `ffi/*`

**Files:**
- Modify: `src/vm/builtins/global.rs`, `src/vm/builtins/iter.rs`, `src/vm/builtins/meta.rs`
- Modify: `src/vm/libs/coroutine.rs`, `src/vm/libs/debug.rs`, `src/vm/libs/http.rs`, `src/vm/libs/task.rs`, `src/vm/libs/ffi/mod.rs`, `src/vm/libs/ffi/ext.rs`, `src/vm/libs/ffi/sdk_tests.rs`

**Interfaces:**
- Consumes `VM::alloc_table`, `VM::alloc_closure`, `VM::table_set_metatable`, `VM::table_set_str`, `VM::table_raw_set`.

- [ ] **Step 1: Route table/closure/metatable mutations in `builtins/*` (`setmetatable`, `pairs`, `global`) through `VM::alloc_*` and `VM::table_set_metatable`.**
- [ ] **Step 2: Route `coroutine.rs`, `debug.rs`, `http.rs`, `task.rs`, and `ffi/*` through `VM::alloc_*` and remove unnecessary `.to_table()` `.to_closure()` calls.**
- [ ] **Step 3: Run `cargo test --lib` and `cargo test --all-features --workspace`.**
- [ ] **Step 4: Commit Task 3.**

---

### Task 4: Migrate Core Execution Paths (`call.rs`, `ops.rs`, `machine.rs`, `interp.rs`, `threaded.rs`)

**Files:**
- Modify: `src/vm/call.rs`, `src/vm/ops.rs`, `src/vm/machine.rs`, `src/vm/interp.rs`, `src/vm/threaded.rs`

**Interfaces:**
- Ensures every bytecode table/closure creation and `SetTable` / `SetField` / `SetIndex` opcode in `interp.rs` and `threaded.rs` routes through `VmHeap` / `VM` heap helpers (or identical inline fast-path helpers backed by `VmHeap` with write-barrier hooks).

- [ ] **Step 1: Migrate `call.rs`, `ops.rs`, `machine.rs`, `interp.rs` constructors and writes to `VmHeap` helpers.**
- [ ] **Step 2: Migrate `threaded.rs` cold/hot allocation & `SetTable` write-barrier hooks cleanly while preserving tail-call optimization.**
- [ ] **Step 3: Audit with `rg` that zero direct `Rc::new(RefCell::new(VmTable` or `Rc::new(VmClosure` exist outside `src/vm/heap.rs`, `src/vm/value.rs`, and unit tests.**
- [ ] **Step 4: Run `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo clippy --release --all-targets --all-features -- -D warnings`, and `cargo test --all-features --workspace`.**
- [ ] **Step 5: Commit Task 4.**
