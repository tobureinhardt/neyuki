//! Lowers a CFG-based [`MirProgram`] into the three-address bytecode [`IrModule`].

use crate::ast::op::{BinOp, UnOp};
use crate::bytecode::proto::UpvalueDesc;
use crate::compiler::ir::block::{IrFunction, IrModule, UpvalSource};
use crate::compiler::ir::inst::{IrInst, SpreadSink, SpreadSource};
use crate::compiler::ir::types::{IrBinaryOp, IrConstant, IrLabel, IrUnaryOp, IrVar};
use crate::compiler::mir::cfg::{
    BasicBlockId, LocalId, MirBody, MirConst, MirProgram, MirSpreadSource, Operand, Place,
    Projection, Rvalue, StatementKind, TerminatorKind,
};

struct MirToIr {
    next_temp: u32,
    current_line: u32,
    instructions: Vec<IrInst>,
}

impl MirToIr {
    fn new(body: &MirBody) -> Self {
        Self {
            next_temp: body.locals.len() as u32,
            current_line: 0,
            instructions: Vec::new(),
        }
    }

    fn var(id: LocalId) -> IrVar {
        IrVar(id.0)
    }

    fn label(bb: BasicBlockId) -> IrLabel {
        IrLabel(bb.0 as usize)
    }

    fn alloc_temp(&mut self) -> IrVar {
        let v = IrVar(self.next_temp);
        self.next_temp += 1;
        v
    }

    fn convert_const(c: &MirConst) -> IrConstant {
        match c {
            MirConst::Nil => IrConstant::Nil,
            MirConst::Bool(b) => IrConstant::Bool(*b),
            MirConst::Int(i) => IrConstant::Int(i.clone()),
            MirConst::Float(f) => IrConstant::Float(*f),
            MirConst::String(s) => IrConstant::String(s.clone()),
        }
    }

    fn convert_binop(op: BinOp) -> IrBinaryOp {
        match op {
            BinOp::Add => IrBinaryOp::Add,
            BinOp::Sub => IrBinaryOp::Sub,
            BinOp::Mul => IrBinaryOp::Mul,
            BinOp::Div => IrBinaryOp::Div,
            BinOp::IDiv => IrBinaryOp::IDiv,
            BinOp::Mod => IrBinaryOp::Mod,
            BinOp::Pow => IrBinaryOp::Pow,
            BinOp::BitAnd => IrBinaryOp::BitAnd,
            BinOp::BitOr => IrBinaryOp::BitOr,
            BinOp::BitXor => IrBinaryOp::BitXor,
            BinOp::Shl => IrBinaryOp::Shl,
            BinOp::Shr => IrBinaryOp::Shr,
            BinOp::LShl => IrBinaryOp::LShl,
            BinOp::LShr => IrBinaryOp::LShr,
            BinOp::Concat => IrBinaryOp::Concat,
            BinOp::Eq => IrBinaryOp::Eq,
            BinOp::Ne => IrBinaryOp::Ne,
            BinOp::Lt => IrBinaryOp::Lt,
            BinOp::Le => IrBinaryOp::Le,
            BinOp::Gt => IrBinaryOp::Gt,
            BinOp::Ge => IrBinaryOp::Ge,
            BinOp::Coalesce | BinOp::And | BinOp::Or => IrBinaryOp::Coalesce,
        }
    }

    fn convert_unop(op: UnOp) -> IrUnaryOp {
        match op {
            UnOp::Neg => IrUnaryOp::Neg,
            UnOp::Not => IrUnaryOp::Not,
            UnOp::Len => IrUnaryOp::Len,
            UnOp::BitNot => IrUnaryOp::BitNot,
        }
    }

    fn materialize_operand(&mut self, op: &Operand) -> IrVar {
        match op {
            Operand::Constant(MirConst::Nil) => {
                let v = self.alloc_temp();
                self.instructions.push(IrInst::LoadNil { dst: v });
                v
            }
            Operand::Constant(c) => {
                let v = self.alloc_temp();
                self.instructions.push(IrInst::LoadConst {
                    dst: v,
                    val: Self::convert_const(c),
                });
                v
            }
            Operand::Copy(place) | Operand::Move(place) => self.read_place(place),
        }
    }

    fn read_place(&mut self, place: &Place) -> IrVar {
        let mut cur = Self::var(place.local);
        for proj in &place.projections {
            let key = match proj {
                Projection::Field(f) => {
                    let k = self.alloc_temp();
                    self.instructions.push(IrInst::LoadConst {
                        dst: k,
                        val: IrConstant::String(f.clone()),
                    });
                    k
                }
                Projection::Index(idx) => self.materialize_operand(idx),
            };
            let next = self.alloc_temp();
            self.instructions.push(IrInst::GetTable {
                dst: next,
                table: cur,
                key,
            });
            cur = next;
        }
        cur
    }

    fn write_place(&mut self, place: &Place, val: IrVar) {
        if place.projections.is_empty() {
            let dst = Self::var(place.local);
            if dst != val {
                self.instructions.push(IrInst::Move { dst, src: val });
            }
            return;
        }
        let mut cur = Self::var(place.local);
        let last_idx = place.projections.len() - 1;
        for proj in &place.projections[..last_idx] {
            let key = match proj {
                Projection::Field(f) => {
                    let k = self.alloc_temp();
                    self.instructions.push(IrInst::LoadConst {
                        dst: k,
                        val: IrConstant::String(f.clone()),
                    });
                    k
                }
                Projection::Index(idx) => self.materialize_operand(idx),
            };
            let next = self.alloc_temp();
            self.instructions.push(IrInst::GetTable {
                dst: next,
                table: cur,
                key,
            });
            cur = next;
        }
        let last_key = match &place.projections[last_idx] {
            Projection::Field(f) => {
                let k = self.alloc_temp();
                self.instructions.push(IrInst::LoadConst {
                    dst: k,
                    val: IrConstant::String(f.clone()),
                });
                k
            }
            Projection::Index(idx) => self.materialize_operand(idx),
        };
        self.instructions.push(IrInst::SetTable {
            table: cur,
            key: last_key,
            val,
        });
    }

    fn convert_spread_source(&mut self, src: &MirSpreadSource) -> SpreadSource {
        match src {
            MirSpreadSource::Vararg => SpreadSource::Vararg,
            MirSpreadSource::Call {
                callee,
                args,
                trailing,
            } => {
                let callee_var = self.materialize_operand(callee);
                let arg_vars = args.iter().map(|a| self.materialize_operand(a)).collect();
                let trailing_ir = trailing
                    .as_ref()
                    .map(|t| Box::new(self.convert_spread_source(t)));
                SpreadSource::Call {
                    callee: callee_var,
                    args: arg_vars,
                    trailing: trailing_ir,
                }
            }
        }
    }

    fn emit_assign_rvalue(&mut self, dst: IrVar, rvalue: &Rvalue) {
        match rvalue {
            Rvalue::Use(Operand::Constant(MirConst::Nil)) => {
                self.instructions.push(IrInst::LoadNil { dst });
            }
            Rvalue::Use(Operand::Constant(c)) => {
                self.instructions.push(IrInst::LoadConst {
                    dst,
                    val: Self::convert_const(c),
                });
            }
            Rvalue::Use(Operand::Copy(p) | Operand::Move(p)) => {
                let src = self.read_place(p);
                if dst != src {
                    self.instructions.push(IrInst::Move { dst, src });
                }
            }
            Rvalue::BinaryOp { op, lhs, rhs } => {
                let l = self.materialize_operand(lhs);
                let r = self.materialize_operand(rhs);
                self.instructions.push(IrInst::BinOp {
                    dst,
                    op: Self::convert_binop(*op),
                    lhs: l,
                    rhs: r,
                });
            }
            Rvalue::UnaryOp { op, src } => {
                let s = self.materialize_operand(src);
                self.instructions.push(IrInst::UnOp {
                    dst,
                    op: Self::convert_unop(*op),
                    src: s,
                });
            }
            Rvalue::Global(name) => {
                self.instructions.push(IrInst::GetGlobal {
                    dst,
                    name: name.clone(),
                });
            }
            Rvalue::Upvalue(idx) => {
                self.instructions
                    .push(IrInst::GetUpval { dst, index: *idx });
            }
            Rvalue::ModuleImport { module, field } => {
                self.instructions.push(IrInst::GetImport {
                    dst,
                    module: module.clone(),
                    field: field.clone(),
                });
            }
            Rvalue::NewTable {
                array_cap,
                fields_cap,
            } => {
                self.instructions.push(IrInst::NewTable {
                    dst,
                    array_cap: *array_cap,
                    fields_cap: *fields_cap,
                });
            }
            Rvalue::Closure { fn_idx, captures } => {
                let ir_caps = captures.iter().map(|c| c.map(Self::var)).collect();
                self.instructions.push(IrInst::Closure {
                    dst,
                    proto_idx: *fn_idx as u16,
                    captures: ir_caps,
                });
            }
            Rvalue::Vararg => {
                self.instructions.push(IrInst::Vararg { dst, count: 1 });
            }
        }
    }

    fn lower_body(&mut self, body: &MirBody) -> IrFunction {
        let mut child_protos = Vec::with_capacity(body.functions.len());
        for child in &body.functions {
            let mut sub = MirToIr::new(child);
            child_protos.push(sub.lower_body(child));
        }

        let block_count = body.blocks.len();
        for (idx, bb) in body.blocks.iter().enumerate() {
            let next_bb_id = body.blocks.get(idx + 1).map(|b| b.id);
            self.instructions.push(IrInst::Label(Self::label(bb.id)));

            for stmt in &bb.statements {
                if stmt.line > 0 && stmt.line != self.current_line {
                    self.current_line = stmt.line;
                    self.instructions.push(IrInst::Line(stmt.line));
                }
                match &stmt.kind {
                    StatementKind::Assign(place, rvalue) => {
                        if place.projections.is_empty() {
                            self.emit_assign_rvalue(Self::var(place.local), rvalue);
                        } else {
                            let tmp = self.alloc_temp();
                            self.emit_assign_rvalue(tmp, rvalue);
                            self.write_place(place, tmp);
                        }
                    }
                    StatementKind::SetGlobal { name, value } => {
                        let src = self.materialize_operand(value);
                        self.instructions.push(IrInst::SetGlobal {
                            name: name.clone(),
                            src,
                        });
                    }
                    StatementKind::SetUpval { index, value } => {
                        let src = self.materialize_operand(value);
                        self.instructions
                            .push(IrInst::SetUpval { index: *index, src });
                    }
                    StatementKind::AppendArray { table, value } => {
                        let src = self.materialize_operand(value);
                        self.instructions.push(IrInst::AppendArray {
                            table: Self::var(*table),
                            src,
                        });
                    }
                    StatementKind::CloseUpvals(vars) => {
                        self.instructions.push(IrInst::CloseUpvals {
                            vars: vars.iter().copied().map(Self::var).collect(),
                        });
                    }
                    StatementKind::VarargMany(dsts) => {
                        self.instructions.push(IrInst::Spread {
                            source: SpreadSource::Vararg,
                            sink: SpreadSink::Values {
                                dsts: dsts.iter().copied().map(Self::var).collect(),
                            },
                        });
                    }
                    StatementKind::DebugLocal { name, local } => {
                        self.instructions.push(IrInst::DebugLocal {
                            name: name.clone(),
                            var: Self::var(*local),
                        });
                    }
                    StatementKind::DebugScopeEnd { locals, keep_alive } => {
                        self.instructions.push(IrInst::DebugScopeEnd {
                            vars: locals.iter().copied().map(Self::var).collect(),
                            keep_alive: *keep_alive,
                        });
                    }
                }
            }

            match &bb.terminator {
                TerminatorKind::Goto(target) => {
                    if idx + 1 >= block_count || next_bb_id != Some(*target) {
                        self.instructions.push(IrInst::Jump(Self::label(*target)));
                    }
                }
                TerminatorKind::SwitchBool {
                    cond,
                    true_bb,
                    false_bb,
                } => {
                    let cond_var = self.materialize_operand(cond);
                    self.instructions.push(IrInst::JumpIfFalse {
                        cond: cond_var,
                        target: Self::label(*false_bb),
                    });
                    if next_bb_id != Some(*true_bb) {
                        self.instructions.push(IrInst::Jump(Self::label(*true_bb)));
                    }
                }
                TerminatorKind::Call {
                    dsts,
                    callee,
                    args,
                    retc,
                    target,
                } => {
                    let callee_var = self.materialize_operand(callee);
                    let arg_vars = args.iter().map(|a| self.materialize_operand(a)).collect();
                    self.instructions.push(IrInst::Call {
                        dsts: dsts.iter().copied().map(Self::var).collect(),
                        callee: callee_var,
                        args: arg_vars,
                        retc: *retc,
                    });
                    if next_bb_id != Some(*target) {
                        self.instructions.push(IrInst::Jump(Self::label(*target)));
                    }
                }
                TerminatorKind::SpreadCall {
                    dsts,
                    callee,
                    fixed_args,
                    trailing,
                    retc,
                    target,
                } => {
                    let callee_var = self.materialize_operand(callee);
                    let fixed_vars = fixed_args
                        .iter()
                        .map(|a| self.materialize_operand(a))
                        .collect();
                    let source = self.convert_spread_source(trailing);
                    self.instructions.push(IrInst::Spread {
                        source,
                        sink: SpreadSink::Call {
                            dsts: dsts.iter().copied().map(Self::var).collect(),
                            callee: callee_var,
                            fixed_args: fixed_vars,
                            retc: *retc,
                        },
                    });
                    if next_bb_id != Some(*target) {
                        self.instructions.push(IrInst::Jump(Self::label(*target)));
                    }
                }
                TerminatorKind::SpreadList {
                    table,
                    source,
                    target,
                } => {
                    let source_ir = self.convert_spread_source(source);
                    self.instructions.push(IrInst::Spread {
                        source: source_ir,
                        sink: SpreadSink::List {
                            table: Self::var(*table),
                        },
                    });
                    if next_bb_id != Some(*target) {
                        self.instructions.push(IrInst::Jump(Self::label(*target)));
                    }
                }
                TerminatorKind::SpreadReturn { values, source } => {
                    let values = values.iter().map(|v| self.materialize_operand(v)).collect();
                    let source = self.convert_spread_source(source);
                    self.instructions.push(IrInst::Spread {
                        source,
                        sink: SpreadSink::Return { values },
                    });
                }
                TerminatorKind::ForPrep {
                    base,
                    limit,
                    step,
                    loop_var,
                    test_bb,
                    body_bb,
                } => {
                    self.instructions.push(IrInst::ForPrep {
                        base: Self::var(*base),
                        limit: Self::var(*limit),
                        step: Self::var(*step),
                        loop_var: Self::var(*loop_var),
                        jump: Self::label(*test_bb),
                    });
                    if next_bb_id != Some(*body_bb) {
                        self.instructions.push(IrInst::Jump(Self::label(*body_bb)));
                    }
                }
                TerminatorKind::ForLoop {
                    base,
                    body_bb,
                    exit_bb,
                } => {
                    self.instructions.push(IrInst::ForLoop {
                        base: Self::var(*base),
                        jump: Self::label(*body_bb),
                    });
                    if next_bb_id != Some(*exit_bb) {
                        self.instructions.push(IrInst::Jump(Self::label(*exit_bb)));
                    }
                }
                TerminatorKind::TForStep {
                    base,
                    state,
                    ctrl,
                    vars,
                    body_bb,
                    exit_bb,
                } => {
                    self.instructions.push(IrInst::TForCall {
                        base: Self::var(*base),
                        state: Self::var(*state),
                        ctrl: Self::var(*ctrl),
                        vars: vars.iter().copied().map(Self::var).collect(),
                    });
                    self.instructions.push(IrInst::TForLoop {
                        base: Self::var(*base),
                        jump: Self::label(*exit_bb),
                    });
                    if next_bb_id != Some(*body_bb) {
                        self.instructions.push(IrInst::Jump(Self::label(*body_bb)));
                    }
                }
                TerminatorKind::Return(ops) => {
                    let vars: Vec<IrVar> =
                        ops.iter().map(|o| self.materialize_operand(o)).collect();
                    self.instructions.push(IrInst::Return(vars));
                }
                TerminatorKind::Unreachable => {
                    let nil = self.alloc_temp();
                    self.instructions.push(IrInst::LoadNil { dst: nil });
                    self.instructions.push(IrInst::Return(vec![nil]));
                }
            }
        }

        let upvalues = body
            .upvalues
            .iter()
            .map(|u| UpvalueDesc {
                in_stack: u.in_stack,
                index: u.index,
            })
            .collect();
        let upvalue_vars = body
            .upvalues
            .iter()
            .map(|u| match (u.levels_up, u.parent_local) {
                (0, Some(local)) => UpvalSource::ParentLocal(Self::var(local)),
                (_, Some(local)) => UpvalSource::Pending {
                    var: Self::var(local),
                    levels_up: u.levels_up,
                },
                (_, None) => UpvalSource::ParentUpvalue,
            })
            .collect();
        let upvalue_names = body.upvalues.iter().map(|u| u.name.clone()).collect();

        IrFunction {
            name: body.name.clone(),
            params: body.params.clone(),
            num_params: body.num_params,
            is_vararg: body.is_vararg,
            instructions: std::mem::take(&mut self.instructions),
            protos: child_protos,
            upvalues,
            upvalue_vars,
            upvalue_names,
            cfg: None,
        }
    }
}

/// Lowers a [`MirProgram`] into a three-address [`IrModule`].
pub fn mir_to_ir(program: &MirProgram) -> IrModule {
    let mut converter = MirToIr::new(&program.main);
    let main = converter.lower_body(&program.main);
    IrModule { main }
}
