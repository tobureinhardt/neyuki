//! Lowers CFG-based [`MirBody`] functions into LLVM IR SSA instructions.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write;

use num_traits::ToPrimitive;

use crate::ast::op::{BinOp, UnOp};
use crate::compiler::llvm::types::{LlvmTypeHint, format_llvm_cstring, sanitize_ident};
use crate::compiler::mir::{
    BasicBlockId, LocalId, MirBody, MirConst, MirProgram, MirSpreadSource, Operand, Place,
    Projection, Rvalue, StatementKind, TerminatorKind,
};

pub struct LlvmModuleCollector {
    pub string_literals: Vec<(String, String, usize)>,
    string_map: HashMap<String, usize>,
    pub user_globals: BTreeSet<String>,
    pub fn_names: HashMap<(usize, usize), String>,
}

impl LlvmModuleCollector {
    pub fn new() -> Self {
        Self {
            string_literals: Vec::new(),
            string_map: HashMap::new(),
            user_globals: BTreeSet::new(),
            fn_names: HashMap::new(),
        }
    }

    pub fn intern_string(&mut self, s: &str) -> (String, usize) {
        if let Some(&idx) = self.string_map.get(s) {
            let entry = &self.string_literals[idx];
            return (entry.0.clone(), entry.2);
        }
        let idx = self.string_literals.len();
        let sym = format!("@.str.{}", idx);
        let (escaped, len) = format_llvm_cstring(s);
        self.string_literals.push((sym.clone(), escaped, len));
        self.string_map.insert(s.to_string(), idx);
        (sym, len)
    }

    pub fn collect_program(&mut self, prog: &MirProgram) {
        self.collect_body(&prog.main, "nyk_main", 0);
    }

    fn collect_body(&mut self, body: &MirBody, llvm_name: &str, scope_id: usize) {
        for (idx, child) in body.functions.iter().enumerate() {
            let child_name = format!(
                "@nyk_fn_{}_{}_{}",
                scope_id,
                idx,
                sanitize_ident(child.name.as_deref().unwrap_or("anon"))
            );
            self.fn_names.insert((scope_id, idx), child_name.clone());
            self.collect_body(child, &child_name[1..], scope_id * 100 + idx + 1);
        }
        let _ = llvm_name;
        for bb in &body.blocks {
            for stmt in &bb.statements {
                if let StatementKind::SetGlobal { name, .. } = &stmt.kind {
                    self.user_globals.insert(name.clone());
                }
            }
        }
    }
}

impl Default for LlvmModuleCollector {
    fn default() -> Self {
        Self::new()
    }
}

pub struct FunctionEmitter<'a> {
    collector: &'a mut LlvmModuleCollector,
    scope_id: usize,
    next_tmp: usize,
    out: String,
    local_is_print_builtin: HashMap<LocalId, bool>,
    self_name: Option<String>,
    self_llvm_sym: String,
    upvalue_names: Vec<String>,
}

impl<'a> FunctionEmitter<'a> {
    pub fn new(collector: &'a mut LlvmModuleCollector, scope_id: usize) -> Self {
        Self {
            collector,
            scope_id,
            next_tmp: 0,
            out: String::new(),
            local_is_print_builtin: HashMap::new(),
            self_name: None,
            self_llvm_sym: String::new(),
            upvalue_names: Vec::new(),
        }
    }

    fn tmp(&mut self) -> String {
        let id = self.next_tmp;
        self.next_tmp += 1;
        format!("%t{}", id)
    }

    fn slot(id: LocalId) -> String {
        format!("%slot_{}", id.0)
    }

    fn bb_name(id: BasicBlockId) -> String {
        format!("bb{}", id.0)
    }

    fn emit_string_ptr(&mut self, s: &str) -> String {
        let (sym, len) = self.collector.intern_string(s);
        let p = self.tmp();
        let _ = writeln!(
            self.out,
            "  {} = getelementptr inbounds [{} x i8], ptr {}, i64 0, i64 0",
            p, len, sym
        );
        p
    }

    fn eval_const(&mut self, c: &MirConst) -> String {
        let dst = self.tmp();
        match c {
            MirConst::Nil => {
                let _ = writeln!(self.out, "  {} = call %NykVal @__nyk_mk_nil()", dst);
            }
            MirConst::Bool(b) => {
                let _ = writeln!(
                    self.out,
                    "  {} = call %NykVal @__nyk_mk_bool(i1 {})",
                    dst, b
                );
            }
            MirConst::Int(bi) => {
                let v = bi.to_i64().unwrap_or(0);
                let _ = writeln!(
                    self.out,
                    "  {} = call %NykVal @__nyk_mk_int(i64 {})",
                    dst, v
                );
            }
            MirConst::Float(f) => {
                let _ = writeln!(
                    self.out,
                    "  {} = call %NykVal @__nyk_mk_float(double 0x{:016X})",
                    dst,
                    f.to_bits()
                );
            }
            MirConst::String(s) => {
                let ptr = self.emit_string_ptr(s);
                let _ = writeln!(
                    self.out,
                    "  {} = call %NykVal @__nyk_mk_str(ptr {})",
                    dst, ptr
                );
            }
        }
        dst
    }

    fn eval_place(&mut self, place: &Place) -> String {
        let mut cur = self.tmp();
        let _ = writeln!(
            self.out,
            "  {} = load %NykVal, ptr {}",
            cur,
            Self::slot(place.local)
        );
        for proj in &place.projections {
            let key = match proj {
                Projection::Field(f) => {
                    let s = self.emit_string_ptr(f);
                    let k = self.tmp();
                    let _ = writeln!(self.out, "  {} = call %NykVal @__nyk_mk_str(ptr {})", k, s);
                    k
                }
                Projection::Index(idx) => self.eval_operand(idx),
            };
            let next = self.tmp();
            let _ = writeln!(
                self.out,
                "  {} = call %NykVal @__nyk_table_get(%NykVal {}, %NykVal {})",
                next, cur, key
            );
            cur = next;
        }
        cur
    }

    fn store_place(&mut self, place: &Place, val_reg: &str) {
        if place.projections.is_empty() {
            let _ = writeln!(
                self.out,
                "  store %NykVal {}, ptr {}",
                val_reg,
                Self::slot(place.local)
            );
            return;
        }
        let mut cur = self.tmp();
        let _ = writeln!(
            self.out,
            "  {} = load %NykVal, ptr {}",
            cur,
            Self::slot(place.local)
        );
        let last = place.projections.len() - 1;
        for proj in &place.projections[..last] {
            let key = match proj {
                Projection::Field(f) => {
                    let s = self.emit_string_ptr(f);
                    let k = self.tmp();
                    let _ = writeln!(self.out, "  {} = call %NykVal @__nyk_mk_str(ptr {})", k, s);
                    k
                }
                Projection::Index(idx) => self.eval_operand(idx),
            };
            let next = self.tmp();
            let _ = writeln!(
                self.out,
                "  {} = call %NykVal @__nyk_table_get(%NykVal {}, %NykVal {})",
                next, cur, key
            );
            cur = next;
        }
        let last_key = match &place.projections[last] {
            Projection::Field(f) => {
                let s = self.emit_string_ptr(f);
                let k = self.tmp();
                let _ = writeln!(self.out, "  {} = call %NykVal @__nyk_mk_str(ptr {})", k, s);
                k
            }
            Projection::Index(idx) => self.eval_operand(idx),
        };
        let _ = writeln!(
            self.out,
            "  call void @__nyk_table_set(%NykVal {}, %NykVal {}, %NykVal {})",
            cur, last_key, val_reg
        );
    }

    fn eval_operand(&mut self, op: &Operand) -> String {
        match op {
            Operand::Constant(c) => self.eval_const(c),
            Operand::Copy(p) | Operand::Move(p) => self.eval_place(p),
        }
    }

    fn eval_rvalue(&mut self, rv: &Rvalue, dst_local: Option<LocalId>) -> String {
        match rv {
            Rvalue::Use(op) => {
                if let (Some(dst), Operand::Copy(p) | Operand::Move(p)) = (dst_local, op)
                    && p.projections.is_empty()
                {
                    let is_p = self
                        .local_is_print_builtin
                        .get(&p.local)
                        .copied()
                        .unwrap_or(false);
                    self.local_is_print_builtin.insert(dst, is_p);
                }
                self.eval_operand(op)
            }
            Rvalue::BinaryOp { op, lhs, rhs } => {
                let l = self.eval_operand(lhs);
                let r = self.eval_operand(rhs);
                let res = self.tmp();
                let helper = match op {
                    BinOp::Add => "__nyk_add",
                    BinOp::Sub => "__nyk_sub",
                    BinOp::Mul => "__nyk_mul",
                    BinOp::Div => "__nyk_div",
                    BinOp::IDiv => "__nyk_idiv",
                    BinOp::Mod => "__nyk_mod",
                    BinOp::BitAnd => "__nyk_bitand",
                    BinOp::BitOr => "__nyk_bitor",
                    BinOp::BitXor => "__nyk_bitxor",
                    BinOp::Shl | BinOp::LShl => "__nyk_shl",
                    BinOp::Shr => "__nyk_shr",
                    BinOp::LShr => "__nyk_lshr",
                    BinOp::Concat => "__nyk_concat",
                    BinOp::Eq => "__nyk_eq",
                    BinOp::Ne => "__nyk_ne",
                    BinOp::Lt => "__nyk_lt",
                    BinOp::Le => "__nyk_le",
                    BinOp::Gt => "__nyk_gt",
                    BinOp::Ge => "__nyk_ge",
                    BinOp::Pow | BinOp::And | BinOp::Or | BinOp::Coalesce => "__nyk_add",
                };
                let _ = writeln!(
                    self.out,
                    "  {} = call %NykVal @{}(%NykVal {}, %NykVal {})",
                    res, helper, l, r
                );
                res
            }
            Rvalue::UnaryOp { op, src } => {
                let s = self.eval_operand(src);
                let res = self.tmp();
                let helper = match op {
                    UnOp::Neg => "__nyk_neg",
                    UnOp::Not => "__nyk_not",
                    UnOp::Len => "__nyk_len",
                    UnOp::BitNot => "__nyk_bitnot",
                };
                let _ = writeln!(
                    self.out,
                    "  {} = call %NykVal @{}(%NykVal {})",
                    res, helper, s
                );
                res
            }
            Rvalue::Global(name) => {
                if name == "print" {
                    if let Some(dst) = dst_local {
                        self.local_is_print_builtin.insert(dst, true);
                    }
                    let res = self.tmp();
                    let _ = writeln!(
                        self.out,
                        "  {} = insertvalue %NykVal {{ i8 7, i64 0 }}, i64 0, 1",
                        res
                    );
                    return res;
                }
                if let Some(dst) = dst_local {
                    self.local_is_print_builtin.insert(dst, false);
                }
                if self.collector.user_globals.contains(name) {
                    let gsym = format!("@__nyk_glob_{}", sanitize_ident(name));
                    let res = self.tmp();
                    let _ = writeln!(self.out, "  {} = load %NykVal, ptr {}", res, gsym);
                    res
                } else {
                    let s = self.emit_string_ptr(name);
                    let res = self.tmp();
                    let _ = writeln!(
                        self.out,
                        "  {} = call %NykVal @__nyk_global_get(ptr {})",
                        res, s
                    );
                    res
                }
            }
            Rvalue::Upvalue(idx) => {
                let res = self.tmp();
                if let (Some(sname), Some(uname)) =
                    (&self.self_name, self.upvalue_names.get(*idx as usize))
                    && sname == uname
                {
                    let _ = writeln!(
                        self.out,
                        "  {} = call %NykVal @__nyk_mk_fn(ptr @{})",
                        res, self.self_llvm_sym
                    );
                } else {
                    let _ = writeln!(
                        self.out,
                        "  {} = call %NykVal @__nyk_upval_get(i32 {})",
                        res, idx
                    );
                }
                res
            }
            Rvalue::ModuleImport { module, field } => {
                let mp = self.emit_string_ptr(module);
                let fp = self.emit_string_ptr(field);
                let res = self.tmp();
                let _ = writeln!(
                    self.out,
                    "  {} = call %NykVal @__nyk_import_get(ptr {}, ptr {})",
                    res, mp, fp
                );
                res
            }
            Rvalue::NewTable {
                array_cap,
                fields_cap,
            } => {
                let res = self.tmp();
                let _ = writeln!(
                    self.out,
                    "  {} = call %NykVal @__nyk_table_new(i32 {}, i32 {})",
                    res, array_cap, fields_cap
                );
                res
            }
            Rvalue::Closure { fn_idx, captures } => {
                let sym = self
                    .collector
                    .fn_names
                    .get(&(self.scope_id, *fn_idx))
                    .cloned()
                    .unwrap_or_else(|| "@nyk_unknown".to_string());
                let res = self.tmp();
                if captures.is_empty() {
                    let _ = writeln!(
                        self.out,
                        "  {} = call %NykVal @__nyk_mk_fn(ptr {})",
                        res, sym
                    );
                } else {
                    let _ = writeln!(
                        self.out,
                        "  {} = call %NykVal @__nyk_closure(ptr {}, i32 {})",
                        res,
                        sym,
                        captures.len()
                    );
                }
                res
            }
            Rvalue::Vararg => {
                let res = self.tmp();
                let _ = writeln!(self.out, "  {} = call %NykVal @__nyk_vararg()", res);
                res
            }
        }
    }

    fn is_print_operand(&self, op: &Operand) -> bool {
        match op {
            Operand::Copy(p) | Operand::Move(p) if p.projections.is_empty() => self
                .local_is_print_builtin
                .get(&p.local)
                .copied()
                .unwrap_or(false),
            _ => false,
        }
    }

    pub fn emit_body(mut self, body: &MirBody, llvm_name: &str) -> String {
        self.self_name = body.name.clone();
        self.self_llvm_sym = llvm_name.to_string();
        self.upvalue_names = body.upvalues.iter().map(|u| u.name.clone()).collect();
        let mut child_defs = String::new();
        for (idx, child) in body.functions.iter().enumerate() {
            let child_name = self
                .collector
                .fn_names
                .get(&(self.scope_id, idx))
                .cloned()
                .unwrap_or_else(|| format!("@nyk_fn_{}_{}", self.scope_id, idx));
            let child_emitter = FunctionEmitter::new(self.collector, self.scope_id * 100 + idx + 1);
            child_defs.push_str(&child_emitter.emit_body(child, &child_name[1..]));
            child_defs.push('\n');
        }

        let params_sig: Vec<String> = (0..body.num_params as usize)
            .map(|i| format!("%NykVal %arg_{}", i))
            .collect();
        let _ = writeln!(
            self.out,
            "define %NykVal @{}({}) {{",
            llvm_name,
            params_sig.join(", ")
        );
        self.out.push_str("entry:\n");

        // Allocate stack slots for all MIR locals so mem2reg/O1 can promote them cleanly.
        for decl in &body.locals {
            let hint = LlvmTypeHint::from_neyuki(&decl.ty).ir_comment_name();
            let _ = writeln!(
                self.out,
                "  {} = alloca %NykVal ; {} {}",
                Self::slot(decl.id),
                hint,
                decl.name.as_deref().unwrap_or("tmp")
            );
            let zero = self.tmp();
            let _ = writeln!(self.out, "  {} = call %NykVal @__nyk_mk_nil()", zero);
            let _ = writeln!(
                self.out,
                "  store %NykVal {}, ptr {}",
                zero,
                Self::slot(decl.id)
            );
        }

        // Bind incoming arguments to their corresponding argument locals.
        let mut arg_idx = 0usize;
        for decl in &body.locals {
            if matches!(decl.kind, crate::compiler::mir::LocalKind::Arg)
                && arg_idx < body.num_params as usize
            {
                let _ = writeln!(
                    self.out,
                    "  store %NykVal %arg_{}, ptr {}",
                    arg_idx,
                    Self::slot(decl.id)
                );
                arg_idx += 1;
            }
        }

        if let Some(first_bb) = body.blocks.first() {
            let _ = writeln!(self.out, "  br label %{}", Self::bb_name(first_bb.id));
        } else {
            self.out.push_str("  ret %NykVal zeroinitializer\n");
        }

        let mut bb_counter = 0usize;
        for bb in &body.blocks {
            let _ = writeln!(self.out, "{}:", Self::bb_name(bb.id));
            for stmt in &bb.statements {
                match &stmt.kind {
                    StatementKind::Assign(place, rvalue) => {
                        let dst_local = if place.projections.is_empty() {
                            Some(place.local)
                        } else {
                            None
                        };
                        let val = self.eval_rvalue(rvalue, dst_local);
                        self.store_place(place, &val);
                    }
                    StatementKind::SetGlobal { name, value } => {
                        let val = self.eval_operand(value);
                        if self.collector.user_globals.contains(name) {
                            let gsym = format!("@__nyk_glob_{}", sanitize_ident(name));
                            let _ = writeln!(self.out, "  store %NykVal {}, ptr {}", val, gsym);
                        } else {
                            let s = self.emit_string_ptr(name);
                            let _ = writeln!(
                                self.out,
                                "  call void @__nyk_global_set(ptr {}, %NykVal {})",
                                s, val
                            );
                        }
                    }
                    StatementKind::SetUpval { index, value } => {
                        let val = self.eval_operand(value);
                        let _ = writeln!(
                            self.out,
                            "  call void @__nyk_upval_set(i32 {}, %NykVal {})",
                            index, val
                        );
                    }
                    StatementKind::AppendArray { table, value } => {
                        let t = self.eval_place(&Place::local(*table));
                        let v = self.eval_operand(value);
                        let _ = writeln!(
                            self.out,
                            "  call void @__nyk_table_append(%NykVal {}, %NykVal {})",
                            t, v
                        );
                    }
                    StatementKind::VarargMany(dsts) => {
                        // This backend models `...` as its first value only.
                        for (i, d) in dsts.iter().enumerate() {
                            let v = self.tmp();
                            let helper = if i == 0 {
                                "__nyk_vararg"
                            } else {
                                "__nyk_mk_nil"
                            };
                            let _ = writeln!(self.out, "  {} = call %NykVal @{}()", v, helper);
                            let _ =
                                writeln!(self.out, "  store %NykVal {}, ptr {}", v, Self::slot(*d));
                        }
                    }
                    StatementKind::CloseUpvals(_)
                    | StatementKind::DebugLocal { .. }
                    | StatementKind::DebugScopeEnd { .. } => {}
                }
            }

            match &bb.terminator {
                TerminatorKind::Goto(target) => {
                    let _ = writeln!(self.out, "  br label %{}", Self::bb_name(*target));
                }
                TerminatorKind::SwitchBool {
                    cond,
                    true_bb,
                    false_bb,
                } => {
                    let c = self.eval_operand(cond);
                    let t = self.tmp();
                    let _ = writeln!(self.out, "  {} = call i1 @__nyk_truthy(%NykVal {})", t, c);
                    let _ = writeln!(
                        self.out,
                        "  br i1 {}, label %{}, label %{}",
                        t,
                        Self::bb_name(*true_bb),
                        Self::bb_name(*false_bb)
                    );
                }
                TerminatorKind::Call {
                    dsts,
                    callee,
                    args,
                    target,
                    ..
                } => {
                    let is_print = self.is_print_operand(callee);
                    let arg_vals: Vec<String> = args.iter().map(|a| self.eval_operand(a)).collect();
                    if is_print {
                        for (i, a) in arg_vals.iter().enumerate() {
                            if i > 0 {
                                self.out.push_str("  call void @__nyk_print_sp()\n");
                            }
                            let _ =
                                writeln!(self.out, "  call void @__nyk_print_one(%NykVal {})", a);
                        }
                        self.out.push_str("  call void @__nyk_print_nl()\n");
                        for d in dsts {
                            let nil = self.tmp();
                            let _ = writeln!(self.out, "  {} = call %NykVal @__nyk_mk_nil()", nil);
                            let _ = writeln!(
                                self.out,
                                "  store %NykVal {}, ptr {}",
                                nil,
                                Self::slot(*d)
                            );
                        }
                    } else {
                        let c_val = self.eval_operand(callee);
                        let tag = self.tmp();
                        let _ = writeln!(self.out, "  {} = extractvalue %NykVal {}, 0", tag, c_val);
                        let is_fn = self.tmp();
                        let _ = writeln!(self.out, "  {} = icmp eq i8 {}, 6", is_fn, tag);
                        let lbl_direct = format!("call_dir_{}", bb_counter);
                        let lbl_dyn = format!("call_dyn_{}", bb_counter);
                        let lbl_join = format!("call_join_{}", bb_counter);
                        bb_counter += 1;

                        let _ = writeln!(
                            self.out,
                            "  br i1 {}, label %{}, label %{}",
                            is_fn, lbl_direct, lbl_dyn
                        );

                        // Direct function-pointer invocation path
                        let _ = writeln!(self.out, "{}:", lbl_direct);
                        let raw_ptr = self.tmp();
                        let _ = writeln!(
                            self.out,
                            "  {} = extractvalue %NykVal {}, 1",
                            raw_ptr, c_val
                        );
                        let fn_ptr = self.tmp();
                        let _ =
                            writeln!(self.out, "  {} = inttoptr i64 {} to ptr", fn_ptr, raw_ptr);
                        let arg_list: Vec<String> =
                            arg_vals.iter().map(|a| format!("%NykVal {}", a)).collect();
                        let direct_res = self.tmp();
                        let _ = writeln!(
                            self.out,
                            "  {} = call %NykVal {}({})",
                            direct_res,
                            fn_ptr,
                            arg_list.join(", ")
                        );
                        let _ = writeln!(self.out, "  br label %{}", lbl_join);

                        // Dynamic fallback path (prints if tag==7, else external __nyk_call_dyn)
                        let _ = writeln!(self.out, "{}:", lbl_dyn);
                        let is_prt = self.tmp();
                        let _ = writeln!(self.out, "  {} = icmp eq i8 {}, 7", is_prt, tag);
                        let lbl_prt = format!("call_prt_{}", bb_counter);
                        let lbl_ext = format!("call_ext_{}", bb_counter);
                        bb_counter += 1;
                        let _ = writeln!(
                            self.out,
                            "  br i1 {}, label %{}, label %{}",
                            is_prt, lbl_prt, lbl_ext
                        );

                        let _ = writeln!(self.out, "{}:", lbl_prt);
                        for (i, a) in arg_vals.iter().enumerate() {
                            if i > 0 {
                                self.out.push_str("  call void @__nyk_print_sp()\n");
                            }
                            let _ =
                                writeln!(self.out, "  call void @__nyk_print_one(%NykVal {})", a);
                        }
                        self.out.push_str("  call void @__nyk_print_nl()\n");
                        let prt_res = self.tmp();
                        let _ = writeln!(self.out, "  {} = call %NykVal @__nyk_mk_nil()", prt_res);
                        let _ = writeln!(self.out, "  br label %{}", lbl_join);

                        let _ = writeln!(self.out, "{}:", lbl_ext);
                        let dyn_res = self.tmp();
                        let _ = writeln!(
                            self.out,
                            "  {} = call %NykVal @__nyk_call_dyn(%NykVal {}, i32 {}, ptr null)",
                            dyn_res,
                            c_val,
                            arg_vals.len()
                        );
                        let _ = writeln!(self.out, "  br label %{}", lbl_join);

                        let _ = writeln!(self.out, "{}:", lbl_join);
                        let merged = self.tmp();
                        let _ = writeln!(
                            self.out,
                            "  {} = phi %NykVal [ {}, %{} ], [ {}, %{} ], [ {}, %{} ]",
                            merged, direct_res, lbl_direct, prt_res, lbl_prt, dyn_res, lbl_ext
                        );
                        if let Some(first_dst) = dsts.first() {
                            let _ = writeln!(
                                self.out,
                                "  store %NykVal {}, ptr {}",
                                merged,
                                Self::slot(*first_dst)
                            );
                        }
                        for extra_dst in dsts.iter().skip(1) {
                            let nil = self.tmp();
                            let _ = writeln!(self.out, "  {} = call %NykVal @__nyk_mk_nil()", nil);
                            let _ = writeln!(
                                self.out,
                                "  store %NykVal {}, ptr {}",
                                nil,
                                Self::slot(*extra_dst)
                            );
                        }
                    }
                    let _ = writeln!(self.out, "  br label %{}", Self::bb_name(*target));
                }
                TerminatorKind::SpreadCall {
                    dsts,
                    callee,
                    fixed_args,
                    trailing,
                    target,
                    ..
                } => {
                    let c_val = self.eval_operand(callee);
                    let _ = trailing;
                    let _ = fixed_args;
                    let dyn_res = self.tmp();
                    let _ = writeln!(
                        self.out,
                        "  {} = call %NykVal @__nyk_call_dyn(%NykVal {}, i32 {}, ptr null)",
                        dyn_res,
                        c_val,
                        fixed_args.len()
                    );
                    if let Some(first_dst) = dsts.first() {
                        let _ = writeln!(
                            self.out,
                            "  store %NykVal {}, ptr {}",
                            dyn_res,
                            Self::slot(*first_dst)
                        );
                    }
                    let _ = writeln!(self.out, "  br label %{}", Self::bb_name(*target));
                }
                TerminatorKind::SpreadList { target, .. } => {
                    let _ = writeln!(self.out, "  br label %{}", Self::bb_name(*target));
                }
                TerminatorKind::SpreadReturn { values, source } => {
                    // Functions here return one value, so the first one wins.
                    let ret_val = match (values.first(), source) {
                        (Some(first), _) => self.eval_operand(first),
                        (None, MirSpreadSource::Call { callee, .. }) => {
                            let c_val = self.eval_operand(callee);
                            let res = self.tmp();
                            let _ = writeln!(
                                self.out,
                                "  {} = call %NykVal @__nyk_call_dyn(%NykVal {}, i32 0, ptr null)",
                                res, c_val
                            );
                            res
                        }
                        (None, MirSpreadSource::Vararg) => {
                            let res = self.tmp();
                            let _ = writeln!(self.out, "  {} = call %NykVal @__nyk_vararg()", res);
                            res
                        }
                    };
                    let _ = writeln!(self.out, "  ret %NykVal {}", ret_val);
                }
                TerminatorKind::ForPrep {
                    base,
                    step,
                    test_bb,
                    ..
                } => {
                    let b = self.eval_place(&Place::local(*base));
                    let s = self.eval_place(&Place::local(*step));
                    let init = self.tmp();
                    let _ = writeln!(
                        self.out,
                        "  {} = call %NykVal @__nyk_sub(%NykVal {}, %NykVal {})",
                        init, b, s
                    );
                    let _ = writeln!(
                        self.out,
                        "  store %NykVal {}, ptr {}",
                        init,
                        Self::slot(*base)
                    );
                    let _ = writeln!(self.out, "  br label %{}", Self::bb_name(*test_bb));
                }
                TerminatorKind::ForLoop {
                    base,
                    body_bb,
                    exit_bb,
                } => {
                    let limit_local = LocalId(base.0 + 1);
                    let step_local = LocalId(base.0 + 2);
                    let loop_var_local = LocalId(base.0 + 3);

                    let b = self.eval_place(&Place::local(*base));
                    let lim = self.eval_place(&Place::local(limit_local));
                    let stp = self.eval_place(&Place::local(step_local));

                    let next = self.tmp();
                    let _ = writeln!(
                        self.out,
                        "  {} = call %NykVal @__nyk_add(%NykVal {}, %NykVal {})",
                        next, b, stp
                    );
                    let _ = writeln!(
                        self.out,
                        "  store %NykVal {}, ptr {}",
                        next,
                        Self::slot(*base)
                    );
                    let _ = writeln!(
                        self.out,
                        "  store %NykVal {}, ptr {}",
                        next,
                        Self::slot(loop_var_local)
                    );

                    // Positive step: next <= limit; negative step: limit <= next.
                    let zero = self.tmp();
                    let _ = writeln!(self.out, "  {} = call %NykVal @__nyk_mk_int(i64 0)", zero);
                    let is_neg_v = self.tmp();
                    let _ = writeln!(
                        self.out,
                        "  {} = call %NykVal @__nyk_lt(%NykVal {}, %NykVal {})",
                        is_neg_v, stp, zero
                    );
                    let is_neg = self.tmp();
                    let _ = writeln!(
                        self.out,
                        "  {} = call i1 @__nyk_truthy(%NykVal {})",
                        is_neg, is_neg_v
                    );

                    let pos_cmp_v = self.tmp();
                    let _ = writeln!(
                        self.out,
                        "  {} = call %NykVal @__nyk_le(%NykVal {}, %NykVal {})",
                        pos_cmp_v, next, lim
                    );
                    let neg_cmp_v = self.tmp();
                    let _ = writeln!(
                        self.out,
                        "  {} = call %NykVal @__nyk_ge(%NykVal {}, %NykVal {})",
                        neg_cmp_v, next, lim
                    );

                    let cond_v = self.tmp();
                    let _ = writeln!(
                        self.out,
                        "  {} = select i1 {}, %NykVal {}, %NykVal {}",
                        cond_v, is_neg, neg_cmp_v, pos_cmp_v
                    );
                    let cond = self.tmp();
                    let _ = writeln!(
                        self.out,
                        "  {} = call i1 @__nyk_truthy(%NykVal {})",
                        cond, cond_v
                    );
                    let _ = writeln!(
                        self.out,
                        "  br i1 {}, label %{}, label %{}",
                        cond,
                        Self::bb_name(*body_bb),
                        Self::bb_name(*exit_bb)
                    );
                }
                TerminatorKind::TForStep {
                    base,
                    vars,
                    body_bb,
                    exit_bb,
                    ..
                } => {
                    let iter_fn = self.eval_place(&Place::local(*base));
                    let first = self.tmp();
                    let _ = writeln!(
                        self.out,
                        "  {} = call %NykVal @__nyk_call_dyn(%NykVal {}, i32 2, ptr null)",
                        first, iter_fn
                    );
                    if let Some(v0) = vars.first() {
                        let _ = writeln!(
                            self.out,
                            "  store %NykVal {}, ptr {}",
                            first,
                            Self::slot(*v0)
                        );
                    }
                    let tag = self.tmp();
                    let _ = writeln!(self.out, "  {} = extractvalue %NykVal {}, 0", tag, first);
                    let is_nil = self.tmp();
                    let _ = writeln!(self.out, "  {} = icmp eq i8 {}, 0", is_nil, tag);
                    let _ = writeln!(
                        self.out,
                        "  br i1 {}, label %{}, label %{}",
                        is_nil,
                        Self::bb_name(*exit_bb),
                        Self::bb_name(*body_bb)
                    );
                }
                TerminatorKind::Return(ops) => {
                    if let Some(first) = ops.first() {
                        let v = self.eval_operand(first);
                        let _ = writeln!(self.out, "  ret %NykVal {}", v);
                    } else {
                        self.out.push_str("  ret %NykVal zeroinitializer\n");
                    }
                }
                TerminatorKind::Unreachable => {
                    self.out.push_str("  ret %NykVal zeroinitializer\n");
                }
            }
        }

        self.out.push_str("}\n");

        let mut combined = child_defs;
        combined.push_str(&self.out);
        combined
    }
}

pub fn emit_user_globals(globals: &BTreeSet<String>) -> String {
    let mut out = String::new();
    for g in globals {
        let _ = writeln!(
            out,
            "@__nyk_glob_{} = internal global %NykVal zeroinitializer",
            sanitize_ident(g)
        );
    }
    if !globals.is_empty() {
        out.push('\n');
    }
    out
}

pub fn emit_string_constants(strings: &[(String, String, usize)]) -> String {
    let mut out = String::new();
    let mut ordered: BTreeMap<&str, (&str, usize)> = BTreeMap::new();
    for (sym, escaped, len) in strings {
        ordered.insert(sym.as_str(), (escaped.as_str(), *len));
    }
    for (sym, (escaped, len)) in ordered {
        let _ = writeln!(
            out,
            "{} = private unnamed_addr constant [{} x i8] {}",
            sym, len, escaped
        );
    }
    if !strings.is_empty() {
        out.push('\n');
    }
    out
}
