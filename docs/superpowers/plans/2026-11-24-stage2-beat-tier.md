# Stage 2: Beat Bump Tier, Nested Watermarks & Loop Liveness Safepoints Implementation Plan

> **For agentic workers:** Execute task-by-task cleanly, keeping every test green and verifying `cargo fmt`, `cargo clippy -- -D warnings` (debug + release), workspace tests, release assembly TCO, and benchmarks.

**Goal:** Enable automatic Beat bump allocation (`BeatTable` + `BeatString`) inside loops and ticks with O(1) rewind, zero overhead on non-allocating numeric loops (`has_alloc == false`), compiler-derived `[u64; 4]` loop liveness masks, `SetTable`/`SetField`/`SetIndex`/`SetUpval`/`SetGlobal` write barriers, nested watermarks across `call_function` (`pcall`/`table.sort`/metamethods), soft-cap spill, and `coroutine.yield` flush — while keeping `Rc<RefCell<VmTable>>` + `GcTracker` as the `Old` backing store ahead of Stage 3.

**Architecture:**
1. Bytecode liveness + loop analysis in `src/bytecode/liveness.rs` wired into `ExecProgram::compile` (`src/bytecode/exec.rs`), leaving serialized `.nykb` bytecode format untouched. Non-allocating loops (`has_alloc == false`) emit zero Beat checks; allocating loops lower to `*Beat` safe-points referencing `[u64; 4]` live register masks.
2. `src/vm/beat.rs` implements `BeatArena` (chunked stable-address bump arena so `BeatTable` headers never relocate mid-Beat), inline/slice `BeatTable` + `BeatString` (zero system `malloc`/`Drop` in common paths, `BeatExtra` fallback for rare `general`/metatable/`>2KB` tables), nested `(floor, depth)` frames, `rem_set`, and deterministic `BeatFinalizers`.
3. `src/vm/value.rs` adds `BIT_BEAT = 1u64 << 47` in the 48-bit payload so `Value::clone`/`Value::drop` skip `Rc` operations for Beat values and write barriers check tier in bitwise operations, plus `TableBorrow` / `TableBorrowMut` unifying `BeatTable` and `RefCell<VmTable>` accesses cleanly across `src/vm/`.
4. `src/vm/heap.rs`, `call.rs`, `libs/coroutine.rs`, `interp.rs`, and `threaded.rs` wire Beat allocation, nested watermark push/pop, `yield` flush, soft-cap spill, write barriers, and Beat evacuation/reset cleanly with zero TCO regressions.

**Tech Stack:** Rust (Edition 2024), 64-bit NaN-boxed `Value(u64)`, `ExecProgram` (`src/bytecode/exec.rs`), `threaded.rs` / `interp.rs`.

**Spec:** `docs/superpowers/specs/2026-11-24-memory-follows-work-design.md` (Section 3, Section 4.2–4.6, Section 5 Stage 2).

## Global Constraints

- Keep `std::mem::size_of::<Value>() == 8` and `std::mem::size_of::<AlignedExec>() == 8`.
- Do NOT break serialized bytecode wire compatibility (`serialize.rs` / `deserialize.rs`).
- Preserve 100% of existing tests, strict `clippy -- -D warnings` (debug & release), and zero broken tail calls (`blr`) in `threaded.rs`.
- Zero `#[allow(...)]` suppressions and zero stub/placeholder code.

---

### Task 1: Bytecode Loop Liveness & `has_alloc` Safepoint Analysis (`src/bytecode/liveness.rs` + `exec.rs`)

**Files:**
- Create: `src/bytecode/liveness.rs`
- Modify: `src/bytecode/mod.rs`, `src/bytecode/exec.rs`

**Interfaces:**
- Produces `LoopSafepoint { pub has_alloc: bool, pub live_mask: [u64; 4], pub loop_start: usize }` and `ExecProgram::safepoint(&self, site: u16) -> Option<&LoopSafepoint>`.
- Adds `ExecInst::ForPrepBeat`, `ForLoopBeat`, `TForLoopBeat`, `JumpBeat` (keeping `AlignedExec` at 8 bytes) selected during `ExecProgram::compile` ONLY when `has_alloc == true`.

- [ ] **Step 1: Implement backward register liveness (`[u64; 4]`) and loop body `has_alloc` detection over `Instruction` slices in `src/bytecode/liveness.rs` with unit tests verifying pure numeric loops have `has_alloc == false` and table-allocating loops record exact live registers.**
- [ ] **Step 2: Wire `src/bytecode/mod.rs` and `src/bytecode/exec.rs` (`ExecInst` Beat variants + `ExecProgram::safepoint`) and temporarily alias Beat variants in `interp.rs`/`threaded.rs` dispatch tables until Task 4 turns on `BeatArena`.**
- [ ] **Step 3: Run `cargo test --lib bytecode` and `cargo clippy --release --lib -- -D warnings`.**
- [ ] **Step 4: Commit Task 1.**

---

### Task 2: `BeatArena`, `BeatTable`, `BeatString`, `BIT_BEAT` in `Value(u64)`, and `TableBorrow` / `TableBorrowMut`

**Files:**
- Create: `src/vm/beat.rs`
- Modify: `src/vm/mod.rs`, `src/vm/value.rs`

**Interfaces:**
- Produces `BIT_BEAT` (`1u64 << 47`), `Value::is_beat()`, `Value::beat_table(ptr)`, `Value::beat_string(ptr)`, `Value::table_borrow()`, `Value::table_borrow_mut()`, `TableBorrow`, `TableBorrowMut`.
- Produces `BeatArena` (chunked non-moving bump slabs, nested `floor`/`depth` watermark stack, `BeatTable` with 4 inline array + 4 inline field slots + bump growth + `BeatExtra` fallback, `BeatString`, `rem_set`, `finalizers`).

- [ ] **Step 1: Write `src/vm/beat.rs` with unit tests covering chunk allocation, `BeatTable` array/field CRUD + `BeatExtra` overflow fallback, nested `(floor, depth)` frames, and O(1) rewind.**
- [ ] **Step 2: Add `BIT_BEAT` handling to `Value` (`Clone`, `Drop`, `is_beat`, `table_borrow`, `table_borrow_mut`, `str_borrow`) in `src/vm/value.rs` with unit tests verifying zero RC ops on Beat values and `Value` remains 8 bytes.**
- [ ] **Step 3: Run `cargo test --lib vm::beat` and `cargo test --lib vm::value`.**
- [ ] **Step 4: Commit Task 2.**

---

### Task 3: Migrate Table Readers/Writers to `TableBorrow` + Implement Beat Evacuation, Nested Watermarks, and `yield` Flush

**Files:**
- Modify: `src/vm/heap.rs`, `src/vm/machine.rs`, `src/vm/call.rs`, `src/vm/metamethod.rs`, `src/vm/table.rs`, `src/vm/upvalue.rs`, `src/vm/libs/*`, `src/vm/builtins/*`, `src/vm/gc.rs`

**Interfaces:**
- Migrates `.as_table()` borrows cleanly to `table_borrow()` / `table_borrow_mut()` / `VmHeap` accessors so Beat tables work transparently everywhere (`pairs`, `next`, `table.*`, `json`, `ffi`, `ext`).
- Produces `VM::beat_enter_loop`, `VM::beat_safepoint_reset(live_mask, mark)`, `VM::beat_enter_nested()`, `VM::beat_exit_nested()`, `VM::beat_flush_all()`, `VM::on_write_barrier(container, val)` (evacuating reachable Beat graphs with forwarding pointers to `mark` / `Old`, dropping non-escaped `BeatExtra`, running `BeatFinalizers`, clearing `!live_mask` Beat registers to `NIL`), plus `coroutine.yield` Beat flush and soft-cap spill (`4 MiB`).

- [ ] **Step 1: Migrate `src/vm/` callers of `.as_table()` to unified `table_borrow`/`table_borrow_mut` + `VmHeap` (promoting/handling FFI extension boundaries to `Old` at `neyuki-ext`).**
- [ ] **Step 2: Implement Beat evacuation + forwarding + nested watermark promotion + `call_function` nested guards + `coroutine.yield` flush in `src/vm/heap.rs` / `src/vm/machine.rs` / `src/vm/call.rs` with targeted unit tests (cycles, DAG aliasing, nested `pcall`/`table.sort`, soft-cap spill, coroutine yield).**
- [ ] **Step 3: Run `cargo test --all-features --workspace`.**
- [ ] **Step 4: Commit Task 3.**

---

### Task 4: Enable Beat Allocation, Write Barriers & Loop Safe-Points in `interp.rs` & `threaded.rs`

**Files:**
- Modify: `src/vm/interp.rs`, `src/vm/threaded.rs`

**Interfaces:**
- Routes `NewTable` (small/medium tables in active Beat) to `vm.alloc_beat_or_old_table_sized`, wires write barriers on `SetTable`, `SetTableK`, `SetList`, `AppendArray`, `SetUpval`, `SetGlobal`, and enables `ForPrepBeat` / `ForLoopBeat` / `TForLoopBeat` / `JumpBeat` safe-points (keeping non-allocating `ForLoop` / `ForLoopInt` / `ForLoopCount` untouched).

- [ ] **Step 1: Wire Beat allocation, write barriers, and `*Beat` loop opcodes into `interp.rs` and `threaded.rs`.**
- [ ] **Step 2: Emit release assembly for `threaded.rs` and verify zero `blr` dispatch regressions on hot handlers.**
- [ ] **Step 3: Run `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo clippy --release --all-targets --all-features -- -D warnings`, `cargo test --all-features --workspace`, `cargo test --release --lib`, `./target/release/neyuki bench`, and `bench_vs_lua54.py`.**
- [ ] **Step 4: Commit Task 4.**
