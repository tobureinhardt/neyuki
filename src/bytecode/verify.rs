//! Checks loaded prototypes before they run: registers, constant indices,
//! jump targets and nested prototypes must all be in range.

use crate::bytecode::instruction::{Instruction, MULTRET};
use crate::bytecode::proto::Proto;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BytecodeVerifyError {
    pub message: String,
    pub proto_name: Option<String>,
    pub pc: Option<usize>,
}

impl std::fmt::Display for BytecodeVerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = self.proto_name.as_deref().unwrap_or("<anonymous>");
        if let Some(pc) = self.pc {
            write!(
                f,
                "bytecode verification failed in '{}' at pc [{}]: {}",
                name, pc, self.message
            )
        } else {
            write!(
                f,
                "bytecode verification failed in '{}': {}",
                name, self.message
            )
        }
    }
}

impl std::error::Error for BytecodeVerifyError {}

const MAX_VERIFY_DEPTH: usize = 512;

pub fn verify_proto(proto: &Proto) -> Result<(), BytecodeVerifyError> {
    verify_proto_depth(proto, 0)
}

fn verify_proto_depth(proto: &Proto, depth: usize) -> Result<(), BytecodeVerifyError> {
    if depth >= MAX_VERIFY_DEPTH {
        return Err(BytecodeVerifyError {
            message: format!(
                "nested prototype depth limit ({}) exceeded",
                MAX_VERIFY_DEPTH
            ),
            proto_name: proto.name.clone(),
            pc: None,
        });
    }

    let name = proto.name.clone();
    let num_constants = proto.constants.len();
    let num_protos = proto.protos.len();
    let num_upvalues = proto.upvalues.len();
    let num_insts = proto.instructions.len();

    // A call clears the parameter registers the caller did not pass, which
    // must lie inside the register window the call reserves.
    if proto.num_params > proto.max_registers {
        return Err(BytecodeVerifyError {
            message: format!(
                "parameter count {} exceeds prototype max_registers ({})",
                proto.num_params, proto.max_registers
            ),
            proto_name: name,
            pc: None,
        });
    }

    let check_reg = |reg: u8, pc: usize| -> Result<(), BytecodeVerifyError> {
        if reg >= proto.max_registers {
            return Err(BytecodeVerifyError {
                message: format!(
                    "register R{} exceeds prototype max_registers ({})",
                    reg, proto.max_registers
                ),
                proto_name: name.clone(),
                pc: Some(pc),
            });
        }
        Ok(())
    };

    let check_const = |k: u16, pc: usize| -> Result<(), BytecodeVerifyError> {
        if k as usize >= num_constants {
            return Err(BytecodeVerifyError {
                message: format!(
                    "constant index K{} exceeds constant pool size ({})",
                    k, num_constants
                ),
                proto_name: name.clone(),
                pc: Some(pc),
            });
        }
        Ok(())
    };

    let check_jump = |offset: i16, pc: usize| -> Result<(), BytecodeVerifyError> {
        let target_ip = pc as isize + 1 + offset as isize;
        if target_ip < 0 || target_ip > num_insts as isize {
            return Err(BytecodeVerifyError {
                message: format!(
                    "jump offset {} targets invalid instruction index {} (total: {})",
                    offset, target_ip, num_insts
                ),
                proto_name: name.clone(),
                pc: Some(pc),
            });
        }
        Ok(())
    };

    // Check that reg + span - 1 does not exceed max_registers
    let check_reg_range = |base: u8, span: usize, pc: usize| -> Result<(), BytecodeVerifyError> {
        let top = base as usize + span;
        if top > proto.max_registers as usize {
            return Err(BytecodeVerifyError {
                message: format!(
                    "register range R{}..R{} exceeds prototype max_registers ({})",
                    base,
                    top - 1,
                    proto.max_registers
                ),
                proto_name: name.clone(),
                pc: Some(pc),
            });
        }
        Ok(())
    };

    for (pc, inst) in proto.instructions.iter().enumerate() {
        match inst {
            Instruction::LoadNil { dst } => {
                check_reg(*dst, pc)?;
            }
            Instruction::LoadBool { dst, .. } => {
                check_reg(*dst, pc)?;
            }
            Instruction::LoadInt { dst, .. } => {
                check_reg(*dst, pc)?;
            }
            Instruction::LoadK { dst, k } => {
                check_reg(*dst, pc)?;
                check_const(*k, pc)?;
            }
            Instruction::Move { dst, src } => {
                check_reg(*dst, pc)?;
                check_reg(*src, pc)?;
            }
            Instruction::Add { dst, a, b }
            | Instruction::Sub { dst, a, b }
            | Instruction::Mul { dst, a, b }
            | Instruction::Div { dst, a, b }
            | Instruction::IDiv { dst, a, b }
            | Instruction::Mod { dst, a, b }
            | Instruction::Pow { dst, a, b }
            | Instruction::BitAnd { dst, a, b }
            | Instruction::BitOr { dst, a, b }
            | Instruction::BitXor { dst, a, b }
            | Instruction::Shl { dst, a, b }
            | Instruction::Shr { dst, a, b }
            | Instruction::LShl { dst, a, b }
            | Instruction::LShr { dst, a, b }
            | Instruction::Concat { dst, a, b } => {
                check_reg(*dst, pc)?;
                check_reg(*a, pc)?;
                check_reg(*b, pc)?;
            }
            Instruction::AddK { dst, a, k }
            | Instruction::SubK { dst, a, k }
            | Instruction::MulK { dst, a, k }
            | Instruction::IDivK { dst, a, k }
            | Instruction::ModK { dst, a, k } => {
                check_reg(*dst, pc)?;
                check_reg(*a, pc)?;
                check_const(*k, pc)?;
            }
            Instruction::Unm { dst, src }
            | Instruction::Not { dst, src }
            | Instruction::Len { dst, src }
            | Instruction::BitNot { dst, src } => {
                check_reg(*dst, pc)?;
                check_reg(*src, pc)?;
            }
            Instruction::Coalesce { dst, a, b } => {
                check_reg(*dst, pc)?;
                check_reg(*a, pc)?;
                check_reg(*b, pc)?;
            }
            Instruction::NewTable { dst, .. } => {
                check_reg(*dst, pc)?;
            }
            Instruction::GetTable { dst, table, key } => {
                check_reg(*dst, pc)?;
                check_reg(*table, pc)?;
                check_reg(*key, pc)?;
            }
            Instruction::SetTable { table, key, val } => {
                check_reg(*table, pc)?;
                check_reg(*key, pc)?;
                check_reg(*val, pc)?;
            }
            Instruction::GetTableK { dst, table, key_k } => {
                check_reg(*dst, pc)?;
                check_reg(*table, pc)?;
                check_const(*key_k, pc)?;
            }
            Instruction::GetImport {
                dst,
                mod_k,
                field_k,
                site: _,
            } => {
                check_reg(*dst, pc)?;
                check_const(*mod_k, pc)?;
                check_const(*field_k, pc)?;
            }
            Instruction::SetTableK { table, key_k, val } => {
                check_reg(*table, pc)?;
                check_const(*key_k, pc)?;
                check_reg(*val, pc)?;
            }
            Instruction::AppendArray { table, src } => {
                check_reg(*table, pc)?;
                check_reg(*src, pc)?;
            }
            Instruction::SetList { table, base, count } => {
                check_reg(*table, pc)?;
                // base .. base+count must all be valid, unless the count comes
                // from the runtime stack top
                if *count != MULTRET {
                    check_reg_range(*base, *count as usize, pc)?;
                }
            }
            Instruction::GetGlobal { dst, name_k } => {
                check_reg(*dst, pc)?;
                check_const(*name_k, pc)?;
            }
            Instruction::SetGlobal { src, name_k } => {
                check_reg(*src, pc)?;
                check_const(*name_k, pc)?;
            }
            Instruction::GetUpval { dst, upval_idx } => {
                check_reg(*dst, pc)?;
                if *upval_idx as usize >= num_upvalues {
                    return Err(BytecodeVerifyError {
                        message: format!(
                            "upvalue index {} exceeds prototype upvalues count ({})",
                            upval_idx, num_upvalues
                        ),
                        proto_name: name.clone(),
                        pc: Some(pc),
                    });
                }
            }
            Instruction::SetUpval { src, upval_idx } => {
                check_reg(*src, pc)?;
                if *upval_idx as usize >= num_upvalues {
                    return Err(BytecodeVerifyError {
                        message: format!(
                            "upvalue index {} exceeds prototype upvalues count ({})",
                            upval_idx, num_upvalues
                        ),
                        proto_name: name.clone(),
                        pc: Some(pc),
                    });
                }
            }
            Instruction::Closure { dst, proto_idx } => {
                check_reg(*dst, pc)?;
                if *proto_idx as usize >= num_protos {
                    return Err(BytecodeVerifyError {
                        message: format!(
                            "closure prototype index {} exceeds protos count ({})",
                            proto_idx, num_protos
                        ),
                        proto_name: name.clone(),
                        pc: Some(pc),
                    });
                }
                let child = &proto.protos[*proto_idx as usize];
                for (up_i, updesc) in child.upvalues.iter().enumerate() {
                    if updesc.in_stack {
                        if updesc.index >= proto.max_registers {
                            return Err(BytecodeVerifyError {
                                message: format!(
                                    "closure proto {} upvalue {} in_stack index R{} exceeds parent max_registers ({})",
                                    proto_idx, up_i, updesc.index, proto.max_registers
                                ),
                                proto_name: name.clone(),
                                pc: Some(pc),
                            });
                        }
                    } else if updesc.index as usize >= num_upvalues {
                        return Err(BytecodeVerifyError {
                            message: format!(
                                "closure proto {} upvalue {} parent upvalue index {} exceeds parent upvalues count ({})",
                                proto_idx, up_i, updesc.index, num_upvalues
                            ),
                            proto_name: name.clone(),
                            pc: Some(pc),
                        });
                    }
                }
            }
            Instruction::Call { callee, argc, retc } => {
                check_reg(*callee, pc)?;
                // callee+1 .. callee+argc are args; callee .. callee+retc-1 are
                // results. A MULTRET count is bounded by the runtime stack top
                // rather than by max_registers, so it is not checked here.
                let args_top = if *argc == MULTRET {
                    0
                } else {
                    (*callee as usize) + 1 + (*argc as usize)
                };
                let rets_top = if *retc == MULTRET {
                    0
                } else {
                    (*callee as usize) + (*retc as usize)
                };
                let max_top = args_top.max(rets_top);
                if max_top > proto.max_registers as usize {
                    return Err(BytecodeVerifyError {
                        message: format!(
                            "call callee R{} with argc {} / retc {} exceeds max_registers ({})",
                            callee, argc, retc, proto.max_registers
                        ),
                        proto_name: name.clone(),
                        pc: Some(pc),
                    });
                }
            }
            Instruction::Return { base, count } => {
                if *count > 0 && *count != MULTRET {
                    // Return range: base .. base+count-1
                    check_reg_range(*base, *count as usize, pc)?;
                }
            }
            Instruction::Vararg { dst, count } => {
                check_reg(*dst, pc)?;
                if *count > 0 && *count != MULTRET {
                    check_reg_range(*dst, *count as usize, pc)?;
                }
            }
            Instruction::Jump { offset } => {
                check_jump(*offset, pc)?;
            }
            Instruction::Test { reg, jump_if_false } => {
                check_reg(*reg, pc)?;
                check_jump(*jump_if_false, pc)?;
            }
            Instruction::Eq {
                a,
                b,
                jump_if_false,
            }
            | Instruction::Ne {
                a,
                b,
                jump_if_false,
            }
            | Instruction::Lt {
                a,
                b,
                jump_if_false,
            }
            | Instruction::Le {
                a,
                b,
                jump_if_false,
            }
            | Instruction::Gt {
                a,
                b,
                jump_if_false,
            }
            | Instruction::Ge {
                a,
                b,
                jump_if_false,
            } => {
                check_reg(*a, pc)?;
                check_reg(*b, pc)?;
                check_jump(*jump_if_false, pc)?;
            }
            Instruction::EqK {
                a,
                k,
                jump_if_false,
            }
            | Instruction::NeK {
                a,
                k,
                jump_if_false,
            }
            | Instruction::LtK {
                a,
                k,
                jump_if_false,
            }
            | Instruction::LeK {
                a,
                k,
                jump_if_false,
            }
            | Instruction::GtK {
                a,
                k,
                jump_if_false,
            }
            | Instruction::GeK {
                a,
                k,
                jump_if_false,
            } => {
                check_reg(*a, pc)?;
                check_const(*k, pc)?;
                check_jump(*jump_if_false, pc)?;
            }
            Instruction::ForPrep { base, jump } => {
                // ForPrep/ForLoop reserve 4 consecutive registers: init, limit, step, loop_var
                check_reg_range(*base, 4, pc)?;
                check_jump(*jump, pc)?;
            }
            Instruction::ForLoop { base, jump } => {
                // ForLoop uses R(base)..R(base+3)
                check_reg_range(*base, 4, pc)?;
                check_jump(*jump, pc)?;
            }
            Instruction::TForCall { base, retc } => {
                // TForCall: R(base)=iter, R(base+1)=state, R(base+2)=ctrl, results at base+3..base+3+retc
                let needed = 3 + (*retc as usize);
                check_reg_range(*base, needed, pc)?;
            }
            Instruction::TForLoop { base, jump } => {
                // TForLoop reads R(base+3) and writes R(base+2)
                check_reg_range(*base, 4, pc)?;
                check_jump(*jump, pc)?;
            }
            Instruction::CloseUpval { reg } => {
                check_reg(*reg, pc)?;
            }
            Instruction::AddI { dst, a, .. }
            | Instruction::SubI { dst, a, .. }
            | Instruction::MulI { dst, a, .. }
            | Instruction::ShlI { dst, a, .. }
            | Instruction::ShrI { dst, a, .. } => {
                check_reg(*dst, pc)?;
                check_reg(*a, pc)?;
            }
            Instruction::EqI {
                a, jump_if_false, ..
            }
            | Instruction::NeI {
                a, jump_if_false, ..
            }
            | Instruction::LtI {
                a, jump_if_false, ..
            }
            | Instruction::LeI {
                a, jump_if_false, ..
            }
            | Instruction::GtI {
                a, jump_if_false, ..
            }
            | Instruction::GeI {
                a, jump_if_false, ..
            } => {
                check_reg(*a, pc)?;
                check_jump(*jump_if_false, pc)?;
            }
        }
    }

    // Recursively verify all nested prototypes with incremented depth
    for child in &proto.protos {
        verify_proto_depth(child, depth + 1)?;
    }

    Ok(())
}
