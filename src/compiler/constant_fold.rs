//! Constant folding on the syntax tree.

use num_bigint::{BigInt, Sign};
use num_integer::Integer as _;
use num_traits::{FromPrimitive, ToPrimitive, Zero};
use std::collections::HashSet;

use crate::ast::literal::Literal;
use crate::ast::op::{BinOp, UnOp};
use crate::ast::pattern::AssignTarget;
use crate::parser::{Expr, InterpPart, Stmt, TableEntry};

#[derive(Clone, Debug)]
enum FoldVal {
    Nil,
    Bool(bool),
    Int(BigInt),
    Float(f64),
    Str(String),
}

impl PartialEq for FoldVal {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (FoldVal::Nil, FoldVal::Nil) => true,
            (FoldVal::Bool(a), FoldVal::Bool(b)) => a == b,
            (FoldVal::Int(a), FoldVal::Int(b)) => a == b,
            (FoldVal::Float(a), FoldVal::Float(b)) => a == b,
            (FoldVal::Int(a), FoldVal::Float(b)) => a.to_f64().is_some_and(|v| v == *b),
            (FoldVal::Float(a), FoldVal::Int(b)) => b.to_f64().is_some_and(|v| *a == v),
            (FoldVal::Str(a), FoldVal::Str(b)) => a == b,
            _ => false,
        }
    }
}

impl FoldVal {
    fn to_value(&self) -> crate::vm::Value {
        use crate::vm::Value;
        match self {
            FoldVal::Nil => Value::Nil,
            FoldVal::Bool(b) => Value::Bool(*b),
            FoldVal::Int(i) => Value::from_bigint(i.clone()),
            FoldVal::Float(f) => Value::Float(*f),
            FoldVal::Str(s) => Value::str(s),
        }
    }

    fn from_value(value: &crate::vm::Value) -> Option<FoldVal> {
        use crate::vm::Value;
        Some(match value {
            Value::Nil => FoldVal::Nil,
            Value::Bool(b) => FoldVal::Bool(*b),
            Value::Int(i) => FoldVal::Int(BigInt::from(*i)),
            Value::BigInt(i) => FoldVal::Int((**i).clone()),
            Value::Float(f) => FoldVal::Float(*f),
            Value::String(s) => FoldVal::Str(s.to_string()),
            _ => return None,
        })
    }

    fn is_truthy(&self) -> bool {
        match self {
            FoldVal::Nil => false,
            FoldVal::Bool(b) => *b,
            _ => true,
        }
    }

    #[allow(clippy::wrong_self_convention)]
    fn to_expr(self) -> Expr {
        match self {
            FoldVal::Nil => Expr::nil(),
            FoldVal::Bool(b) => Expr::bool(b),
            FoldVal::Int(i) => Expr::int(i),
            FoldVal::Float(f) => Expr::float(f),
            FoldVal::Str(s) => Expr::string(s),
        }
    }

    fn from_expr(expr: &Expr) -> Option<Self> {
        match expr {
            Expr::Literal { value: lit, .. } => match lit {
                Literal::Nil => Some(FoldVal::Nil),
                Literal::Bool(b) => Some(FoldVal::Bool(*b)),
                Literal::Int(i) => Some(FoldVal::Int(i.clone())),
                Literal::Float(f) => Some(FoldVal::Float(*f)),
                Literal::String(s) => Some(FoldVal::Str(s.clone())),
            },
            _ => None,
        }
    }

    fn to_f64(&self) -> Option<f64> {
        match self {
            FoldVal::Float(f) => Some(*f),
            FoldVal::Int(i) => i.to_f64(),
            _ => None,
        }
    }

    fn to_bigint(&self) -> Option<BigInt> {
        match self {
            FoldVal::Int(i) => Some(i.clone()),
            FoldVal::Float(f) => BigInt::from_f64(f.trunc()),
            _ => None,
        }
    }
}

// Fold constant unary operations
fn fold_unary_op(op: UnOp, val: FoldVal) -> Option<FoldVal> {
    match op {
        UnOp::Neg => match val {
            FoldVal::Int(i) => Some(FoldVal::Int(-i)),
            FoldVal::Float(f) => Some(FoldVal::Float(-f)),
            _ => None,
        },
        UnOp::Not => Some(FoldVal::Bool(!val.is_truthy())),
        UnOp::Len => match val {
            FoldVal::Str(s) => Some(FoldVal::Int(BigInt::from(s.len()))),
            _ => None,
        },
        UnOp::BitNot => match val {
            FoldVal::Int(i) => Some(FoldVal::Int(!i)),
            FoldVal::Float(f) => {
                let bi = BigInt::from_f64(f.trunc())?;
                Some(FoldVal::Int(!bi))
            }
            _ => None,
        },
    }
}

// Fold constant binary operations
/// Whether folding `a op b` stays within the runtime's integer size limit.
/// Anything larger is left for runtime, which reports it as an error instead
/// of the compiler trying to build it.
fn int_fold_fits(op: BinOp, a: &BigInt, b: &BigInt) -> bool {
    use crate::vm::ops::{MAX_INT_BITS, pow_result_bits};
    let bits = match op {
        BinOp::Add | BinOp::Sub => a.bits().max(b.bits()) + 1,
        BinOp::Mul => a.bits() + b.bits(),
        BinOp::Pow => match b.to_u64() {
            Some(exp) => pow_result_bits(a, exp),
            None => return true,
        },
        BinOp::Shl | BinOp::Shr => {
            let widens = (op == BinOp::Shl) == (b.sign() != Sign::Minus);
            if !widens || a.is_zero() {
                return true;
            }
            a.bits()
                .saturating_add(b.magnitude().to_u64().unwrap_or(u64::MAX))
        }
        _ => return true,
    };
    bits <= MAX_INT_BITS
}

fn fold_binary_op(op: BinOp, left: FoldVal, right: FoldVal) -> Option<FoldVal> {
    if let (FoldVal::Int(a), FoldVal::Int(b)) = (&left, &right)
        && !int_fold_fits(op, a, b)
    {
        return None;
    }
    match op {
        BinOp::Add => match (left, right) {
            (FoldVal::Int(a), FoldVal::Int(b)) => Some(FoldVal::Int(a + b)),
            (a, b) => {
                let fa = a.to_f64()?;
                let fb = b.to_f64()?;
                Some(FoldVal::Float(fa + fb))
            }
        },
        BinOp::Sub => match (left, right) {
            (FoldVal::Int(a), FoldVal::Int(b)) => Some(FoldVal::Int(a - b)),
            (a, b) => {
                let fa = a.to_f64()?;
                let fb = b.to_f64()?;
                Some(FoldVal::Float(fa - fb))
            }
        },
        BinOp::Mul => match (left, right) {
            (FoldVal::Int(a), FoldVal::Int(b)) => Some(FoldVal::Int(a * b)),
            (a, b) => {
                let fa = a.to_f64()?;
                let fb = b.to_f64()?;
                Some(FoldVal::Float(fa * fb))
            }
        },
        BinOp::Div => {
            let fa = left.to_f64()?;
            let fb = right.to_f64()?;
            if fb == 0.0 {
                return None;
            }
            Some(FoldVal::Float(fa / fb))
        }
        BinOp::IDiv => match (left, right) {
            (FoldVal::Int(a), FoldVal::Int(b)) => {
                if b.is_zero() {
                    return None;
                }
                Some(FoldVal::Int(a.div_floor(&b)))
            }
            (a, b) => {
                let fa = a.to_f64()?;
                let fb = b.to_f64()?;
                if fb == 0.0 {
                    return None;
                }
                Some(FoldVal::Float((fa / fb).floor()))
            }
        },
        BinOp::Mod => match (left, right) {
            (FoldVal::Int(a), FoldVal::Int(b)) => {
                if b.is_zero() {
                    return None;
                }
                Some(FoldVal::Int(a.mod_floor(&b)))
            }
            (a, b) => {
                let fa = a.to_f64()?;
                let fb = b.to_f64()?;
                if fb == 0.0 {
                    return None;
                }
                Some(FoldVal::Float(fa - (fa / fb).floor() * fb))
            }
        },
        BinOp::Pow => match (left, right) {
            // Integer powers stay integers, as they do at runtime; folding
            // them through f64 would silently lose precision past 2^53.
            (FoldVal::Int(a), FoldVal::Int(b)) if b.sign() != Sign::Minus => {
                // Huge exponents are left for runtime rather than built here.
                const MAX_FOLDED_EXPONENT: u32 = 4096;
                let exp = b.to_u32().filter(|e| *e <= MAX_FOLDED_EXPONENT)?;
                Some(FoldVal::Int(a.pow(exp)))
            }
            (a, b) => {
                let fa = a.to_f64()?;
                let fb = b.to_f64()?;
                Some(FoldVal::Float(fa.powf(fb)))
            }
        },
        BinOp::BitAnd => {
            let ia = left.to_bigint()?;
            let ib = right.to_bigint()?;
            Some(FoldVal::Int(ia & ib))
        }
        BinOp::BitOr => {
            let ia = left.to_bigint()?;
            let ib = right.to_bigint()?;
            Some(FoldVal::Int(ia | ib))
        }
        BinOp::BitXor => {
            let ia = left.to_bigint()?;
            let ib = right.to_bigint()?;
            Some(FoldVal::Int(ia ^ ib))
        }
        BinOp::Shl => {
            let ia = left.to_bigint()?;
            let ib = right.to_bigint()?;
            // Float operands skipped the check above.
            if !int_fold_fits(op, &ia, &ib) {
                return None;
            }
            if ib.sign() == Sign::Minus {
                let shift = (-ib).to_usize()?;
                Some(FoldVal::Int(ia >> shift))
            } else {
                let shift = ib.to_usize()?;
                Some(FoldVal::Int(ia << shift))
            }
        }
        BinOp::Shr => {
            let ia = left.to_bigint()?;
            let ib = right.to_bigint()?;
            // Float operands skipped the check above.
            if !int_fold_fits(op, &ia, &ib) {
                return None;
            }
            if ib.sign() == Sign::Minus {
                let shift = (-ib).to_usize()?;
                Some(FoldVal::Int(ia << shift))
            } else {
                let shift = ib.to_usize()?;
                Some(FoldVal::Int(ia >> shift))
            }
        }
        BinOp::LShl => {
            let ia = left.to_bigint()?;
            let ib = right.to_bigint()?;
            let word = (ia & BigInt::from(u64::MAX)).to_u64().unwrap_or(0);
            let bits = ib.to_usize().unwrap_or(usize::MAX);
            let res = if bits >= 64 { 0 } else { word << bits };
            Some(FoldVal::Int(BigInt::from(res)))
        }
        BinOp::LShr => {
            let ia = left.to_bigint()?;
            let ib = right.to_bigint()?;
            let word = (ia & BigInt::from(u64::MAX)).to_u64().unwrap_or(0);
            let bits = ib.to_usize().unwrap_or(usize::MAX);
            let res = if bits >= 64 { 0 } else { word >> bits };
            Some(FoldVal::Int(BigInt::from(res)))
        }
        BinOp::Concat => {
            let sa = match left {
                FoldVal::Str(s) => s,
                FoldVal::Int(i) => i.to_string(),
                FoldVal::Float(f) => f.to_string(),
                _ => return None,
            };
            let sb = match right {
                FoldVal::Str(s) => s,
                FoldVal::Int(i) => i.to_string(),
                FoldVal::Float(f) => f.to_string(),
                _ => return None,
            };
            Some(FoldVal::Str(format!("{}{}", sa, sb)))
        }
        BinOp::Eq => Some(FoldVal::Bool(left == right)),
        BinOp::Ne => Some(FoldVal::Bool(left != right)),
        BinOp::Lt => match (&left, &right) {
            (FoldVal::Int(a), FoldVal::Int(b)) => Some(FoldVal::Bool(a < b)),
            (FoldVal::Str(a), FoldVal::Str(b)) => Some(FoldVal::Bool(a < b)),
            _ => {
                let fa = left.to_f64()?;
                let fb = right.to_f64()?;
                Some(FoldVal::Bool(fa < fb))
            }
        },
        BinOp::Le => match (&left, &right) {
            (FoldVal::Int(a), FoldVal::Int(b)) => Some(FoldVal::Bool(a <= b)),
            (FoldVal::Str(a), FoldVal::Str(b)) => Some(FoldVal::Bool(a <= b)),
            _ => {
                let fa = left.to_f64()?;
                let fb = right.to_f64()?;
                Some(FoldVal::Bool(fa <= fb))
            }
        },
        BinOp::Gt => match (&left, &right) {
            (FoldVal::Int(a), FoldVal::Int(b)) => Some(FoldVal::Bool(a > b)),
            (FoldVal::Str(a), FoldVal::Str(b)) => Some(FoldVal::Bool(a > b)),
            _ => {
                let fa = left.to_f64()?;
                let fb = right.to_f64()?;
                Some(FoldVal::Bool(fa > fb))
            }
        },
        BinOp::Ge => match (&left, &right) {
            (FoldVal::Int(a), FoldVal::Int(b)) => Some(FoldVal::Bool(a >= b)),
            (FoldVal::Str(a), FoldVal::Str(b)) => Some(FoldVal::Bool(a >= b)),
            _ => {
                let fa = left.to_f64()?;
                let fb = right.to_f64()?;
                Some(FoldVal::Bool(fa >= fb))
            }
        },
        BinOp::And | BinOp::Or | BinOp::Coalesce => None,
    }
}

// Fold constant builtin calls like math.abs, bit.band, string.len, etc.
fn fold_builtin_call(
    callee: &Expr,
    args: &[FoldVal],
    shadowed: &HashSet<String>,
) -> Option<FoldVal> {
    match callee {
        Expr::Variable { name, .. } => {
            if shadowed.contains(name.as_str()) {
                return None;
            }
            match name.as_str() {
                "type" => {
                    let arg = args.first()?;
                    let t_str = match arg {
                        FoldVal::Nil => "nil",
                        FoldVal::Bool(_) => "boolean",
                        FoldVal::Int(_) | FoldVal::Float(_) => "number",
                        FoldVal::Str(_) => "string",
                    };
                    Some(FoldVal::Str(t_str.to_string()))
                }
                "typeof" => {
                    let arg = args.first()?;
                    let t_str = match arg {
                        FoldVal::Nil => "nil",
                        FoldVal::Bool(_) => "boolean",
                        FoldVal::Int(_) => "bigint",
                        FoldVal::Float(_) => "float",
                        FoldVal::Str(_) => "string",
                    };
                    Some(FoldVal::Str(t_str.to_string()))
                }
                "tostring" => {
                    let arg = args.first()?;
                    let s = match arg {
                        FoldVal::Nil => "nil".to_string(),
                        FoldVal::Bool(b) => b.to_string(),
                        FoldVal::Int(i) => i.to_string(),
                        FoldVal::Float(f) => f.to_string(),
                        FoldVal::Str(s) => s.clone(),
                    };
                    Some(FoldVal::Str(s))
                }
                _ => None,
            }
        }
        Expr::Member { object, field, .. } => {
            if let Expr::Variable { name: mod_name, .. } = object.as_ref() {
                if shadowed.contains(mod_name.as_str()) {
                    return None;
                }
                match (mod_name.as_str(), field.as_str()) {
                    ("bit" | "bit32", name) => {
                        // Folded with the natives' own 32-bit semantics. Float
                        // arguments convert differently there, so only integer
                        // constants are folded.
                        let ints = args
                            .iter()
                            .map(|a| match a {
                                FoldVal::Int(i) => Some(i.clone()),
                                _ => None,
                            })
                            .collect::<Option<Vec<_>>>()?;
                        crate::vm::libs::bit::fold_call(name, &ints).map(FoldVal::Int)
                    }
                    (module, name) => {
                        let values = args.iter().map(FoldVal::to_value).collect::<Vec<_>>();
                        crate::vm::libs::fold_call(module, name, &values)
                            .and_then(|v| FoldVal::from_value(&v))
                    }
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

fn fold_assign_target_scoped(target: AssignTarget, shadowed: &HashSet<String>) -> AssignTarget {
    match target {
        AssignTarget::Variable(name) => AssignTarget::Variable(name),
        AssignTarget::Member { object, field } => AssignTarget::Member {
            object: Box::new(fold_expr_scoped(*object, shadowed)),
            field,
        },
        AssignTarget::Index { object, index } => AssignTarget::Index {
            object: Box::new(fold_expr_scoped(*object, shadowed)),
            index: Box::new(fold_expr_scoped(*index, shadowed)),
        },
    }
}

// Recursively optimize an AST expression by folding constants with shadow tracking
fn fold_expr_scoped(expr: Expr, shadowed: &HashSet<String>) -> Expr {
    match expr {
        Expr::Literal { .. } | Expr::Variable { .. } | Expr::Vararg { .. } => expr,
        Expr::Interp { parts, id } => {
            let folded_parts: Vec<InterpPart> = parts
                .into_iter()
                .map(|p| match p {
                    InterpPart::Literal(s) => InterpPart::Literal(s),
                    InterpPart::Expr(e) => InterpPart::Expr(fold_expr_scoped(e, shadowed)),
                })
                .collect();
            let all_literals = folded_parts
                .iter()
                .all(|p| matches!(p, InterpPart::Literal(_)));
            if all_literals {
                let mut combined = String::new();
                for p in folded_parts {
                    if let InterpPart::Literal(s) = p {
                        combined.push_str(&s);
                    }
                }
                Expr::string(combined)
            } else {
                Expr::Interp {
                    parts: folded_parts,
                    id,
                }
            }
        }
        Expr::Unary {
            op,
            expr: inner,
            id,
        } => {
            let folded_inner = fold_expr_scoped(*inner, shadowed);
            if let Some(c) = FoldVal::from_expr(&folded_inner)
                && let Some(res) = fold_unary_op(op, c)
            {
                return res.to_expr();
            }
            Expr::Unary {
                op,
                expr: Box::new(folded_inner),
                id,
            }
        }
        Expr::Binary {
            left,
            op,
            right,
            id,
        } => {
            let folded_left = fold_expr_scoped(*left, shadowed);

            // Short-circuit folding for 'and', 'or', and '??'
            if op == BinOp::And {
                if let Some(c) = FoldVal::from_expr(&folded_left) {
                    if !c.is_truthy() {
                        return folded_left;
                    }
                    return fold_expr_scoped(*right, shadowed);
                }
            } else if op == BinOp::Or {
                if let Some(c) = FoldVal::from_expr(&folded_left) {
                    if c.is_truthy() {
                        return folded_left;
                    }
                    return fold_expr_scoped(*right, shadowed);
                }
            } else if op == BinOp::Coalesce
                && let Some(c) = FoldVal::from_expr(&folded_left)
            {
                if c != FoldVal::Nil {
                    return folded_left;
                }
                return fold_expr_scoped(*right, shadowed);
            }

            let folded_right = fold_expr_scoped(*right, shadowed);

            if let (Some(cl), Some(cr)) = (
                FoldVal::from_expr(&folded_left),
                FoldVal::from_expr(&folded_right),
            ) && let Some(res) = fold_binary_op(op, cl, cr)
            {
                return res.to_expr();
            }

            Expr::Binary {
                left: Box::new(folded_left),
                op,
                right: Box::new(folded_right),
                id,
            }
        }
        Expr::Member { object, field, id } => Expr::Member {
            object: Box::new(fold_expr_scoped(*object, shadowed)),
            field,
            id,
        },
        Expr::Index { object, index, id } => Expr::Index {
            object: Box::new(fold_expr_scoped(*object, shadowed)),
            index: Box::new(fold_expr_scoped(*index, shadowed)),
            id,
        },
        Expr::Call { callee, args, id } => {
            let folded_callee = fold_expr_scoped(*callee, shadowed);
            let folded_args: Vec<Expr> = args
                .into_iter()
                .map(|a| fold_expr_scoped(a, shadowed))
                .collect();

            // Check if all arguments are constant foldables
            let mut const_args = Vec::with_capacity(folded_args.len());
            let mut all_const = true;
            for a in &folded_args {
                if let Some(c) = FoldVal::from_expr(a) {
                    const_args.push(c);
                } else {
                    all_const = false;
                    break;
                }
            }

            if all_const && let Some(res) = fold_builtin_call(&folded_callee, &const_args, shadowed)
            {
                return res.to_expr();
            }

            if let Some(res) = crate::compiler::builtins_folding::fold_builtin_call_scoped(
                &folded_callee,
                &folded_args,
                shadowed,
            ) {
                return res;
            }

            Expr::Call {
                callee: Box::new(folded_callee),
                args: folded_args,
                id,
            }
        }
        Expr::Function { params, body, id } => {
            let mut child_shadowed = shadowed.clone();
            for p in &params {
                child_shadowed.insert(p.name.clone());
            }
            Expr::Function {
                params,
                body: fold_block(body, &mut child_shadowed),
                id,
            }
        }
        Expr::MethodCall {
            object,
            method,
            args,
            id,
        } => Expr::MethodCall {
            object: Box::new(fold_expr_scoped(*object, shadowed)),
            method,
            args: args
                .into_iter()
                .map(|a| fold_expr_scoped(a, shadowed))
                .collect(),
            id,
        },
        Expr::Table { entries, id } => Expr::Table {
            entries: entries
                .into_iter()
                .map(|e| TableEntry {
                    key: e.key,
                    computed: e.computed.map(|k| Box::new(fold_expr_scoped(*k, shadowed))),
                    value: fold_expr_scoped(e.value, shadowed),
                })
                .collect(),
            id,
        },
    }
}

pub fn fold_expr(expr: Expr) -> Expr {
    fold_expr_scoped(expr, &HashSet::new())
}

fn fold_block(stmts: Vec<Stmt>, shadowed: &mut HashSet<String>) -> Vec<Stmt> {
    let mut out = Vec::new();
    for stmt in stmts {
        if let Some(s) = fold_stmt_scoped(stmt, shadowed) {
            out.push(s);
        }
    }
    out
}

fn fold_stmt_scoped(stmt: Stmt, shadowed: &mut HashSet<String>) -> Option<Stmt> {
    match stmt {
        Stmt::Local {
            name,
            is_const,
            is_global,
            type_name,
            initializer,
            id,
        } => {
            let new_init = initializer.map(|e| fold_expr_scoped(e, shadowed));
            shadowed.insert(name.clone());
            Some(Stmt::Local {
                name,
                is_const,
                is_global,
                type_name,
                initializer: new_init,
                id,
            })
        }
        Stmt::LocalMany {
            names,
            is_const,
            is_global,
            initializers,
            id,
        } => {
            let new_inits = initializers
                .into_iter()
                .map(|e| fold_expr_scoped(e, shadowed))
                .collect();
            for n in &names {
                shadowed.insert(n.clone());
            }
            Some(Stmt::LocalMany {
                names,
                is_const,
                is_global,
                initializers: new_inits,
                id,
            })
        }
        Stmt::Assign {
            target,
            value,
            is_const,
            id,
        } => Some(Stmt::Assign {
            target: fold_assign_target_scoped(target, shadowed),
            value: fold_expr_scoped(value, shadowed),
            is_const,
            id,
        }),
        Stmt::Increment { target, amount, id } => Some(Stmt::Increment {
            target: fold_assign_target_scoped(target, shadowed),
            amount,
            id,
        }),
        Stmt::Function {
            name,
            is_const,
            params,
            return_type,
            body,
            id,
        } => {
            let mut child_shadowed = shadowed.clone();
            for p in &params {
                child_shadowed.insert(p.name.clone());
            }
            if let Some(n) = &name {
                shadowed.insert(n.clone());
            }
            Some(Stmt::Function {
                name,
                is_const,
                params,
                return_type,
                body: fold_block(body, &mut child_shadowed),
                id,
            })
        }
        Stmt::If { data, id } => {
            let folded_cond = fold_expr_scoped(data.condition, shadowed);

            // If static truthy condition, dead-branch elimination can happen
            if let Some(c) = FoldVal::from_expr(&folded_cond)
                && c.is_truthy()
            {
                let mut child_shadowed = shadowed.clone();
                let folded_then = fold_block(data.then_branch, &mut child_shadowed);
                return Some(Stmt::if_stmt(
                    folded_cond,
                    folded_then,
                    Vec::new(),
                    None,
                    id,
                ));
            }

            let mut then_shadowed = shadowed.clone();
            let folded_then = fold_block(data.then_branch, &mut then_shadowed);

            let folded_else_ifs = data
                .else_if_branches
                .into_iter()
                .map(|(cond, branch)| {
                    let c = fold_expr_scoped(cond, shadowed);
                    let mut b_shadowed = shadowed.clone();
                    (c, fold_block(branch, &mut b_shadowed))
                })
                .collect();

            let folded_else = data.else_branch.map(|eb| {
                let mut e_shadowed = shadowed.clone();
                fold_block(eb, &mut e_shadowed)
            });

            Some(Stmt::if_stmt(
                folded_cond,
                folded_then,
                folded_else_ifs,
                folded_else,
                id,
            ))
        }
        Stmt::While {
            condition,
            body,
            id,
        } => {
            let folded_cond = fold_expr_scoped(condition, shadowed);
            if let Some(FoldVal::Bool(false)) | Some(FoldVal::Nil) =
                FoldVal::from_expr(&folded_cond)
            {
                // Eliminate while false loop entirely
                return None;
            }
            let mut body_shadowed = shadowed.clone();
            Some(Stmt::While {
                condition: folded_cond,
                body: fold_block(body, &mut body_shadowed),
                id,
            })
        }
        Stmt::NumericFor { data, id } => {
            let f_start = fold_expr_scoped(data.start, shadowed);
            let f_end = fold_expr_scoped(data.end, shadowed);
            let f_step = data.step.map(|s| fold_expr_scoped(s, shadowed));
            let mut body_shadowed = shadowed.clone();
            body_shadowed.insert(data.var.clone());
            Some(Stmt::numeric_for(
                data.var,
                f_start,
                f_end,
                f_step,
                fold_block(data.body, &mut body_shadowed),
                id,
            ))
        }
        Stmt::For {
            vars,
            source,
            body,
            id,
        } => {
            let f_source = fold_expr_scoped(source, shadowed);
            let mut body_shadowed = shadowed.clone();
            for v in &vars {
                body_shadowed.insert(v.clone());
            }
            Some(Stmt::For {
                vars,
                source: f_source,
                body: fold_block(body, &mut body_shadowed),
                id,
            })
        }
        Stmt::Repeat {
            body,
            condition,
            id,
        } => {
            let mut body_shadowed = shadowed.clone();
            let folded_body = fold_block(body, &mut body_shadowed);
            let folded_cond = fold_expr_scoped(condition, &body_shadowed);
            Some(Stmt::Repeat {
                body: folded_body,
                condition: folded_cond,
                id,
            })
        }
        Stmt::Return { values: exprs, id } => Some(Stmt::Return {
            values: exprs
                .into_iter()
                .map(|e| fold_expr_scoped(e, shadowed))
                .collect(),
            id,
        }),
        Stmt::Expr { expr, id } => Some(Stmt::Expr {
            expr: fold_expr_scoped(expr, shadowed),
            id,
        }),
        Stmt::Break { id } => Some(Stmt::Break { id }),
        Stmt::Continue { id } => Some(Stmt::Continue { id }),
        Stmt::Goto { label, id } => Some(Stmt::Goto { label, id }),
        Stmt::Label { name, id } => Some(Stmt::Label { name, id }),
        alias @ Stmt::TypeAlias { .. } => Some(alias),
        Stmt::AssignMany {
            targets,
            values,
            id,
        } => Some(Stmt::AssignMany {
            targets: targets
                .into_iter()
                .map(|t| fold_assign_target_scoped(t, shadowed))
                .collect(),
            values: values
                .into_iter()
                .map(|e| fold_expr_scoped(e, shadowed))
                .collect(),
            id,
        }),
    }
}

// Optimize statement by folding constant expressions within it
pub fn fold_stmt(stmt: Stmt) -> Option<Stmt> {
    fold_stmt_scoped(stmt, &mut HashSet::new())
}

// Optimize entire program statements with fixpoint convergence
pub fn fold_program(mut stmts: Vec<Stmt>) -> Vec<Stmt> {
    const MAX_FOLD_ROUNDS: usize = 16;
    for _ in 0..MAX_FOLD_ROUNDS {
        let mut shadowed = HashSet::new();
        let new_stmts = fold_block(stmts.clone(), &mut shadowed);
        if new_stmts == stmts {
            return new_stmts;
        }
        stmts = new_stmts;
    }
    stmts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Parser;

    #[test]
    fn test_fold_arithmetic() {
        let mut parser = Parser::new("local x = 10 + 20 * 3");
        let (stmts, _pool) = parser.parse_program().expect("syntax error");
        let optimized = fold_program(stmts);
        match &optimized[0] {
            Stmt::Local {
                initializer:
                    Some(Expr::Literal {
                        value: Literal::Int(val),
                        ..
                    }),
                ..
            } => {
                assert_eq!(val, &BigInt::from(70));
            }
            _ => panic!("failed to fold arithmetic"),
        }
    }

    #[test]
    fn test_fold_bitwise() {
        let mut parser = Parser::new("local x = (1 << 4) | (16 >> 2)");
        let (stmts, _pool) = parser.parse_program().expect("syntax error");
        let optimized = fold_program(stmts);
        // (1 << 4) = 16, (16 >> 2) = 4, 16 | 4 = 20
        match &optimized[0] {
            Stmt::Local {
                initializer:
                    Some(Expr::Literal {
                        value: Literal::Int(val),
                        ..
                    }),
                ..
            } => {
                assert_eq!(val, &BigInt::from(20));
            }
            _ => panic!("failed to fold bitwise"),
        }
    }

    #[test]
    fn huge_shifts_by_a_float_are_left_for_runtime() {
        // Folding these once built a 2^60-bit integer and aborted the
        // process; runtime reports them as a catchable error instead.
        for source in ["local x = 1 << 2^60", "local x = 1 >> -(2.0^62)"] {
            let mut parser = Parser::new(source);
            let (stmts, _pool) = parser.parse_program().expect("syntax error");
            let optimized = fold_program(stmts);
            assert!(
                !matches!(
                    &optimized[0],
                    Stmt::Local {
                        initializer: Some(Expr::Literal { .. }),
                        ..
                    }
                ),
                "{} was folded",
                source
            );
        }
    }

    #[test]
    fn test_fold_builtin_math_and_bit() {
        let mut parser = Parser::new("local a = math.abs(-42)\nlocal b = bit.band(255, 15)");
        let (stmts, _pool) = parser.parse_program().expect("syntax error");
        let optimized = fold_program(stmts);
        match &optimized[0] {
            Stmt::Local {
                initializer:
                    Some(Expr::Literal {
                        value: Literal::Int(val),
                        ..
                    }),
                ..
            } => {
                assert_eq!(val, &BigInt::from(42));
            }
            _ => panic!("failed to fold math.abs"),
        }
        match &optimized[1] {
            Stmt::Local {
                initializer:
                    Some(Expr::Literal {
                        value: Literal::Int(val),
                        ..
                    }),
                ..
            } => {
                assert_eq!(val, &BigInt::from(15));
            }
            _ => panic!("failed to fold bit.band"),
        }
    }

    #[test]
    fn test_fold_does_not_fold_shadowed_math() {
        let mut parser = Parser::new("local math = {}\nlocal a = math.abs(-42)");
        let (stmts, _pool) = parser.parse_program().expect("syntax error");
        let optimized = fold_program(stmts);
        // math.abs(-42) should remain as a call expression because `math` is shadowed!
        match &optimized[1] {
            Stmt::Local {
                initializer: Some(Expr::Call { .. }),
                ..
            } => {}
            _ => panic!("shadowed math was incorrectly folded into a constant!"),
        }
    }

    #[test]
    fn test_fold_equality_int_float_and_float_modulo() {
        let mut parser = Parser::new("local eq = 1 == 1.0\nlocal mod_val = -5.5 % 2.0");
        let (stmts, _pool) = parser.parse_program().expect("syntax error");
        let optimized = fold_program(stmts);
        match &optimized[0] {
            Stmt::Local {
                initializer:
                    Some(Expr::Literal {
                        value: Literal::Bool(val),
                        ..
                    }),
                ..
            } => {
                assert_eq!(val, &true);
            }
            _ => panic!("failed to fold 1 == 1.0"),
        }
        match &optimized[1] {
            Stmt::Local {
                initializer:
                    Some(Expr::Literal {
                        value: Literal::Float(val),
                        ..
                    }),
                ..
            } => {
                assert!((val - 0.5).abs() < 1e-6);
            }
            _ => panic!("failed to fold -5.5 % 2.0"),
        }
    }
}
