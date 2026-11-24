# Memory Follows Work — Architecture Design for Neyuki VM

**Status:** Approved design (pre-implementation, Stage 0 validation next)  
**Target:** `/root/neyuki-upstream` runtime (`src/vm/`, `src/compiler/ir/codegen.rs`, `src/bytecode/proto.rs`)  
**Prerequisite Baseline:** 64-bit Negative-QNaN `Value(u64)` in `src/vm/value.rs` (commit `e29c238`)

---

## 1. Core Thesis & Non-Negotiable Constraints

*Memory follows work. Objects created within a beat die with the beat; long-lived objects belong to their storage owner; no pass ever scans the whole VM heap.*

This design replaces per-object `Rc` reference counting and the global mark-and-sweep cycle collector (`src/vm/gc.rs`) with three execution-scoped memory tiers—**Beat**, **Job**, and **Old**—while resolving four real-VM problems uncovered during prototyping:

1. **Aliasing semantics without silent data loss or annotation tax:** ordinary aliasing (roughly 5% of heap references in dynamic code) must not silently free live data via implicit weak references, yet 95% single-owner nodes must stay free of reference counting and global tracing.
2. **Safe-point rooting with nested native callbacks (`pcall`, `table.sort`, metamethods):** `VM::native_calls > 0` occurs ubiquitously inside Neyuki scripts; disabling Beat resets under `native_calls > 0` would cripple Beat across real programs.
3. **Stale loop-body registers:** local registers holding dead temporaries from iteration `i` remain physically present on `vm.stack` at loop back-edges; naive `[frame_base..live_top]` scanning would falsely retain them in `Old`.
4. **Real VM types, fast-path barriers, peak memory, and coroutines:** Neyuki tables own `array`, `FieldMap`, `general`, `metatable`, `co_state`, and strings; `SetTable` must stay fast; single-beat bursts and `json.decode` cannot overflow Beat; suspended coroutines must preserve Beat values safely across `yield`/`resume`.

---

## 2. Three Memory Tiers

| Tier | Scope & Role | Allocation | Destruction / Reclamation | User API |
|---|---|---|---|---|
| **Beat** | Scratchpad for one loop iteration or task tick (supports nesting via watermarks) | Bump pointer into contiguous chunk | `beat.cursor = mark` at beat end in O(1); survivors evacuate to outer Beat / `Job` / `Old` | Automatic (VM-managed) |
| **Job** | Private heap owned by a scoped unit of work or coroutine | Bump + slot arena | Entire region freed O(1) on job exit / coroutine death (`__gc` runs deterministically) | Builtin `job(fn)` (and implicit per-coroutine) |
| **Old** | Long-lived shared/rooted state (globals, module exports, escaped data, huge bulk values) | Generational `SlotArena` (`slot: u32, gen: u16`) | Immediate subtree reclamation on owner overwrite (`UniqueOwner`), or RC=0 / local cycle collection (`Shared`) | Builtins `share(x)`, `look(x)` |

### Builtin Surface (No Parser Changes)
- `Beat` requires **zero syntax or API**.
- `job(fn)`: executes `fn()` inside a fresh child `Job` region, returns escaped result(s), and reclaims the `Job` heap on return (including error unwind via `pcall`).
- `share(x)`: explicitly promotes/pins `x` in `Old` as `Shared`.
- `look(x)`: returns a non-owning generational handle (`(slot, gen)`) to an `Old`/`Job` object; reads return the target if `arena[slot].gen == gen`, else `nil`.
- `send(target_job, msg)` (when cross-job channels are active): transfers an isolated `Job` sub-region / serialized payload in O(1) without deep copying.

---

## 3. 64-Bit NaN-Boxed `Value(u64)` Encoding

Building directly on Neyuki's existing 64-bit `Value(u64)`:

- **Scalars (`bits < TAG_HEAP_START`):** `f64` (non-NaN + canonical NaN `0x7FF8_0000_0000_0000`), inline 48-bit `Int` (`0xFFF8`), `Nil`/`Bool` (`0xFFF9`), `&'static NativeDef` (`0xFFFA`). Never traced, never require write barriers.
- **Heap references (`bits >= TAG_HEAP_START`):**
  - Tag bits identify kind (`String`, `Table`, `Closure`, `Buffer`, `BigInt`/`HeapInt`, `Look`).
  - **2 tier bits (or payload high bit `BIT_BEAT` + `depth` header)** distinguish **Beat pointers** from **Job/Old SlotIds** directly from the 64-bit `Value` register word without pointer chasing on scalar or same-tier writes.
- **Exempt Immutable / Static Types:**
  - Compile-time constant strings, interned strings, `NativeDef`, `Proto`, and `frozen` tables (module exports, standard library namespaces, metatables once frozen) **never participate in ownership trees or auto-`look`**. Reads/aliases to them copy the handle freely and never break.

---

## 4. Mechanism Details & Solutions to All Weaknesses

### 4.1 Ownership Semantics in `Old` (Auto-Promote `Shared` + Explicit `look`)
To eliminate the prototype's 6.6% semantic corruption on aliased references while keeping 95% of nodes free of RC:

1. **Stack & Upvalue Reads Never Compete for Ownership:** registers on `vm.stack` and open upvalues hold non-owning execution references. Overwriting an `Old` field when a register still holds the old child defers freeing the subtree until Beat safe-point cleanup (`pending_free_old`), preventing `local tmp = a.x; a.x = nil; b.x = tmp` from freeing `tmp`.
2. **First `Old` Heap Store = `UniqueOwner`:** the object header records `State::UniqueOwner` (`rc_or_state = UNIQUE`). Overwriting or deleting the owner slot immediately queues/reclaims the entire owned `UniqueOwner` subtree back to size-classed free lists and runs `__gc` destructors deterministically in reverse allocation order.
3. **Second `Old` Heap Store = Automatic Promotion to `Shared(rc = 2)`:** when an already-owned `Old` object is stored into a second `Old` container (array slot, `FieldMap`, `general`, closed upvalue, or metatable), it automatically promotes from `UniqueOwner` to `Shared(rc = 2)` instead of silently downgrading to a fragile `look`.
   - Cost: 95% single-owner objects never execute RC inc/dec and still free whole subtrees immediately.
   - Only ~5% multi-owner `Shared` objects use `rc` inc/dec on `Old`-to-`Old` stores/overwrites.
4. **Honest Accounting of Cycle Collection (Local `Shared`-Only Collector):**
   - Intentional cycles (parent pointers, caches, observer graphs) should use `look(x)` or weak-value tables (`__mode = "v"` checked with `(slot, gen)`), incurring **zero** cycle-collection work.
   - Accidental cycles among `Shared` objects are handled by a **bounded Bacon-Rajan trial-deletion pass that visits ONLY the `Shared` purple buffer** (populated exclusively when a `Shared` object's `rc` decrements while remaining `> 0`).
   - It **never** scans `Beat`, `UniqueOwner` trees, `vm.stack`, or `globals`. If a codebase creates cycles solely through `look`/`__mode = "v"`, the purple buffer stays empty.

### 4.2 Write Barrier on `SetTable` / `SetField` / `SetIndex` / `SetUpval`
On every container write `container[k] = val`:
1. `if !val.is_heap_ref() { return; }` -> 1 unsigned integer check; scalars cost zero.
2. `if val.is_beat() {` -> checked purely on `val`'s 64-bit tag bit:
   - If `!container.is_beat()` (`Old` or `Job` container receiving a `Beat` value): record `(container_handle, key_or_slot)` in `vm.beat.remembered_set`.
   - If `container.is_beat()` and `val_depth > container_depth`: record in the nested depth's `remembered_set`.
3. `if !val.is_beat() && !container.is_beat()` (`Old` -> `Old` store): transition `val` via `on_old_store(val)` (`Unowned -> UniqueOwner -> Shared(rc += 1)`).

### 4.3 Nested Beats via Watermark (`pcall`, `table.sort`, Metamethods)
Suppression of Beat resets whenever `vm.native_calls > 0` would disable Beat across every loop running inside `pcall`, `table.sort`, or metamethods. Instead, **Beats nest using watermarks**:

- `BeatArena` maintains a stack of frames `(floor: usize, mark: usize, depth: u16)`.
- Before `call_function` enters a native function or Neyuki callback (e.g., `builtin_pcall`, `table_sort`), the VM pushes a nested Beat floor at `floor = beat.cursor` and `depth += 1`.
- Any Beat object created by caller code sits at offset `< floor` (`obj_depth <= caller_depth`) and **never moves** while the native frame is active—even if a Rust native like `table_sort` temporarily holds `&Vec<Value>` (via `std::mem::take`) across `cmp` callbacks.
- Inside the callback's own loops, inner Beat iterations set `mark >= floor` and reset `beat.cursor = mark` at every iteration.
- When an inner Beat object (`offset >= mark`) escapes to an object at depth `<= caller_depth` (below `mark`), evacuation at the inner reset **copies only that survivor directly into `[mark..]` and advances `mark`** (and `floor` if escaping across the native boundary to `caller_depth`), then resets `beat.cursor = new_mark`. Objects below `mark` remain stationary at their addresses.
- **Soft Cap Spill (e.g., 4 MiB):** if a single Beat grows past the chunk soft cap, `[floor..cursor]` spills/evacuates to `Old` (or an overflow slab pinned for that Beat depth) without moving `[0..floor)`.
- **Large / Bulk Native Allocations:** operations that allocate large or dynamically growing graphs natively (`json.decode` above a threshold, `string.rep`, large `VmBuffer`) allocate directly in `Old` (or current `Job`) rather than Beat, guaranteeing no mid-native evacuation can occur.

### 4.4 Real `VmTable` & `VmString` Inside Beat (Zero System `malloc` / `Drop`)
To avoid leaking Rust `Vec`/`Box`/`String` allocations when resetting `beat.cursor = mark` in O(1):

- **`BeatString`:** stores `{ header: u32/u64 len + fx_hash, bytes: [u8] }` directly inline in the Beat bump buffer.
- **`BeatTable`:** stores a fixed header plus inline array slots (e.g., 4 `Value`s) and inline small-field entries (`(StrHash/Ptr, Value)` pairs) allocated from the Beat bump buffer.
  - Growing past inline capacity allocates larger contiguous slices inside Beat up to a small threshold (e.g., 4 KiB).
  - Exceeding the Beat table cap promotes the table once to `Old`/`Job` (where normal growable storage lives).
  - Consequently, short-lived `{ x = i, y = i + 1 }` Beat tables perform **zero system `malloc`/`free` calls and zero `Drop` executions**.

### 4.5 Eliminating Stale Registers via Compiler Liveness Bitmasks
At loop back-edges, temporaries defined inside the loop body still occupy stack registers even though they are dead on the next iteration. Instead of inserting extra `ClearReg` opcodes into tight arithmetic loops:

1. `src/compiler/ir/codegen.rs` already builds `LivenessInfo` (`src/compiler/ir/liveness.rs`) and maps `IrVar` -> `u8` via `RegAlloc` (`[u64; 4]` bitset in `src/compiler/ir/regalloc.rs`).
2. During bytecode generation, each loop safe-point (`ForLoop`, `TForLoop`, and backward `Jump`) records `live_mask: [u64; 4]` (all registers mapped to variables in `live_in(header_block)`) indexed by bytecode PC on `Proto`.
3. At runtime on every loop back-edge:
   - **Zero-allocation fast path:** `if beat.cursor == mark`, nothing was allocated in Beat during this iteration -> skip root scanning and dead-register clearing in **1 predictable branch**.
   - **Reset path (`beat.cursor != mark`):** scan ONLY registers whose bit is `1` in `live_mask` (+ open upvalues + `remembered_set`), reset `beat.cursor = mark`, and clear any dead register (`bit == 0` in `live_mask` within `0..max_registers`) whose tag is `Beat` back to `Value::NIL`. Stale temporaries can never pollute `Old` or become dangling pointers.

### 4.6 Coroutines & Deterministic Finalizers
- **Coroutines:** Each `CoroutineState` binds to its own `Job` context (or inherits the active `Job`). On `coroutine.yield(...)`, the yielding coroutine flushes live Beat references on its suspended stack, `parked_upvalues`, and `yield_values` into its `Job` heap, resets the VM Beat back to `floor`, and yields cleanly. When a coroutine handle is destroyed (owner overwritten / `Job` ends), its `Job` heap frees in O(1).
- **Deterministic `__gc`:** Tables with a callable `__gc` metatable entry register into the owning tier's finalizer stack (`BeatFinalizers`, `JobFinalizers`, or `Old` subtree finalizer walk) and run newest-first deterministically when that Beat resets, Job finishes, or Old owner subtree is reclaimed.
- **Weak Tables (`__mode = "v"`):** Store generational `look` handles `(slot, gen)`; lookups returning stale generations transparently clean the entry and yield `nil`.

---

## 5. Staged Migration Plan & Verification Criteria

1. **Stage 0 — Real-Layout Micro-Benchmark Harness (Current Gate):**
   - Measure `BeatTable` (`array` + `FieldMap` bump layout) vs `Rc<RefCell<VmTable>>` under early-death loops, surviving escapes, `SetTable` tag-barrier cost, nested watermark `pcall`/`table.sort`, stale-register `live_mask` fast skip, and `Auto-Shared` + local trial-deletion cycle stress.
2. **Stage 1 — `VmHeap` / Handle Abstraction Layer:**
   - Isolate remaining direct `Rc<RefCell<VmTable>>` / `StrRef` / `Rc<VmClosure>` usage inside `src/vm/libs/*` and `src/vm/builtins/*` behind `Value`/`VM` handle methods while keeping `Rc` underneath. Gate: 100% of workspace tests pass.
3. **Stage 2 — `Beat` Bump Tier & Compiler `live_mask`:**
   - Enable `Beat` tag bit in `Value(u64)`, `BeatTable`/`BeatString`, `SetTable` barrier, nested watermark for `call_function`, and `Proto` loop `live_mask`. Gate: full test suite + assembly check (zero TCO regressions) + benchmark suite pass.
4. **Stage 3 — `Job` + `Old` `SlotArena`, `Auto-Shared`, and Removal of `src/vm/gc.rs`:**
   - Replace `Rc` storage with `Old` `SlotArena` + `Job` arenas + local `Shared` trial-deletion + builtins (`job`, `share`, `look`) + deterministic `__gc`, then delete the global mark-and-sweep cycle collector. Gate: full test suite, leak/cycle fuzzers, and Lua 5.4 comparison benchmarks.
