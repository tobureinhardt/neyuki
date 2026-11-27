# Stage 3 Implementation Plan: `Old` `SlotArena` + `Job` Arenas + `look`/`share`/`job` + Remove `src/vm/gc.rs`

**Spec:** `docs/superpowers/specs/2026-11-24-memory-follows-work-design.md` (Sections 2, 3, 4.1, 4.2, 4.6, 5.4)
**Baseline:** `main` at `7953832` (Stage 2 `BeatArena` complete)

## Stage Breakdown

### Task 1: `OldArena` (`src/vm/old.rs`)
- Slot-indexed arena `(slot: u32, gen: u16)` (`gen` in `1..=0x7FFF`) with free-list and size-classed `VmTable` buffer reuse.
- Ownership state machine per slot: `Free`, `Unowned` (`rc = 0`), `UniqueOwner` (`rc = 1`), `Shared` (`rc >= 2`).
- Deferred `pending_free_old` queue so register aliases survive field overwrites until safe-points.
- Deterministic iterative subtree reclamation on `UniqueOwner` death with reverse-`alloc_id` `__gc` destructors and resurrection guard.
- Local Bacon-Rajan cycle collector seeded only from `purple_buffer` (`Shared` rc decremented while `> 0`) and `suspect_roots` (container with existing `Old` children transitioning `0 -> 1` or self-cycle), traversing both `Shared` and `UniqueOwner` edges plus closed `Upvalue` / `Closure` graphs, pinned against live stack/global/ext roots.
- Generational `Look(slot, gen)` resolution + stale check.
- Unit & adversarial tests covering self-cycles, 2/3-node cycles, mixed `Shared <-> UniqueOwner` cycles, `pending_free` stack alias safety, subtree `__gc` order, resurrection, weak `(slot, gen)` staleness, and zero false collection of live cyclic objects held on stack.

### Task 2: `Value` Arena Encoding & `JobArena` (`src/vm/value.rs`, `src/vm/job.rs`)
- Introduce `BIT_NON_RC` (`1 << 47`) in `Value(u64)`:
  - `TAG_TABLE | BIT_NON_RC | (ptr | 1)` for `Old`/`Job` tables (zero `Clone`/`Drop` refcount overhead while preserving pointer-direct `BorrowedTable` / `as_table()`).
  - `is_beat()` matches only `bit 47 == 1 && bit 0 == 0`, preserving exact Beat/non-Beat write-barrier distinction in 2 instructions.
  - `TAG_MISC | BIT_NON_RC | ((gen15) << 32) | slot32` for `Look(slot, gen)` handles (`< TAG_RC_START`, zero `Clone`/`Drop`).
  - Preserve `Rc<RefCell<VmTable>>` (`bit 47 == 0`) exclusively for exempt static/frozen/external tables.
- Add `VmTable::arena_id: u64` (`0` = untracked/frozen, nonzero = `(gen, slot)`) for O(1) slot lookup without hash tables.
- Implement `JobManager` (`src/vm/job.rs`): nested `Job` regions (`job_id >= 1`), O(1) allocation tracking, transitive escape promotion (`child_job -> parent_job / Old`), O(1) teardown on job/coroutine exit with reverse-`alloc_id` `__gc`.

### Task 3: Builtins (`job`, `share`, `look`), Barriers, Coroutines, Weak Tables
- Add `src/vm/builtins/mem.rs` implementing `job(fn, ...)`, `share(x)`, `look(x)` (`ValueView::Look`, callable `w()` deref, `w[k]` lookup), registered in `builtins/mod.rs`.
- Replace `VM.gc: GcTracker` with `VmHeap.old` (`OldArena`) + `VmHeap.jobs` (`JobManager`) across `machine.rs`, `heap.rs`, `interp.rs`, `threaded.rs`, `upvalue.rs`, `coroutine.rs`, `call.rs`, `builtins/gc.rs`, `builtins/meta.rs`, `builtins/global.rs`, `libs/ffi/ext.rs`.
- Wire `on_old_store`, `on_old_release`, `__mode = "v"` `(slot, gen)` weak values (including dynamic `mt.__mode = "v"`), closed `Upvalue` tracking, and coroutine per-`Job` binding.

### Task 4: Remove `src/vm/gc.rs`, Adversarial Verification & Benchmarks
- Remove `src/vm/gc.rs` and any remaining dead shims.
- Add end-to-end integration tests for `job`, `share`, `look`, coroutine job teardown, mixed cycles, weak tables, and finalizer order.
- Run full verification gate:
  - `cargo fmt --all -- --check`
  - `cargo clippy --all-targets --all-features -- -D warnings` (debug + `--release`)
  - `cargo test --all-features --workspace` & `cargo test --release --lib`
  - Release assembly TCO check (`0 blr` in `threaded.rs`)
  - `./target/release/neyuki bench` & `bench_vs_lua54.py`
