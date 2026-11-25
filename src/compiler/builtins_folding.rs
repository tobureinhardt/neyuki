use num_bigint::BigInt;
use std::str::FromStr;

use crate::vm::Value;

use crate::ast::Literal;
use crate::parser::Expr;

pub fn fold_builtin_call(callee: &Expr, args: &[Expr]) -> Option<Expr> {
    fold_builtin_call_scoped(callee, args, &std::collections::HashSet::new())
}

pub fn fold_builtin_call_scoped(
    callee: &Expr,
    args: &[Expr],
    shadowed: &std::collections::HashSet<String>,
) -> Option<Expr> {
    let (mod_name, func_name) = match callee {
        Expr::Member { object, field, .. } => match &**object {
            Expr::Variable { name, .. } => {
                if shadowed.contains(name.as_str()) {
                    return None;
                }
                (name.as_str(), field.as_str())
            }
            _ => return None,
        },
        Expr::Variable { name, .. } => {
            if shadowed.contains(name.as_str()) {
                return None;
            }
            // Global builtins e.g. int(), float(), type()
            ("", name.as_str())
        }
        _ => return None,
    };

    match (mod_name, func_name) {
        ("", "int") => fold_builtin_int(args),
        ("", _) => None,
        // `bit.*` is folded by `constant_fold` with the natives' 32-bit
        // semantics (`vm::libs::bit::fold_call`), never here.
        (module, name) => {
            let values = args.iter().map(literal_value).collect::<Option<Vec<_>>>()?;
            value_expr(&crate::vm::libs::fold_call(module, name, &values)?)
        }
    }
}

fn literal_value(expr: &Expr) -> Option<Value> {
    match expr {
        Expr::Literal { value, .. } => Some(match value {
            Literal::Nil => Value::NIL,
            Literal::Bool(b) => Value::bool(*b),
            Literal::Int(i) => Value::from_bigint(i.clone()),
            Literal::Float(f) => Value::float(*f),
            Literal::String(s) => Value::str(s),
        }),
        _ => None,
    }
}

fn value_expr(value: &Value) -> Option<Expr> {
    use crate::vm::value::ValueView;
    Some(match value.view() {
        ValueView::Nil => Expr::nil(),
        ValueView::Bool(b) => Expr::bool(b),
        ValueView::Int(i) => Expr::int(i),
        ValueView::BigInt(i) => Expr::int(i.clone()),
        ValueView::Float(f) => Expr::float(f),
        ValueView::String(s) => Expr::string(s.to_string()),
        _ => return None,
    })
}

fn get_int(expr: &Expr) -> Option<BigInt> {
    match expr {
        Expr::Literal {
            value: Literal::Int(i),
            ..
        } => Some(i.clone()),
        _ => None,
    }
}

fn get_float(expr: &Expr) -> Option<f64> {
    match expr {
        Expr::Literal {
            value: Literal::Float(f),
            ..
        } => Some(*f),
        _ => None,
    }
}

fn get_str(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Literal {
            value: Literal::String(s),
            ..
        } => Some(s.as_str()),
        _ => None,
    }
}

fn fold_builtin_int(args: &[Expr]) -> Option<Expr> {
    let arg = args.first()?;
    if let Some(i) = get_int(arg) {
        return Some(Expr::int(i));
    }
    if let Some(f) = get_float(arg) {
        return Some(Expr::int(f as i64));
    }
    if let Some(i) = get_str(arg).and_then(|s| BigInt::from_str(s).ok()) {
        return Some(Expr::int(i));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::NodeId;

    #[test]
    fn test_math_folding() {
        let abs_call = Expr::Member {
            object: Box::new(Expr::Variable {
                name: "math".to_string(),
                id: NodeId::next(),
            }),
            field: "abs".to_string(),
            id: NodeId::next(),
        };
        let res = fold_builtin_call(&abs_call, &[Expr::int(-42)]);
        assert_eq!(res, Some(Expr::int(42)));

        let sqrt_call = Expr::Member {
            object: Box::new(Expr::Variable {
                name: "math".to_string(),
                id: NodeId::next(),
            }),
            field: "sqrt".to_string(),
            id: NodeId::next(),
        };
        let res = fold_builtin_call(&sqrt_call, &[Expr::float(16.0)]);
        assert_eq!(res, Some(Expr::float(4.0)));
    }

    #[test]
    fn test_string_folding() {
        let len_call = Expr::Member {
            object: Box::new(Expr::Variable {
                name: "string".to_string(),
                id: NodeId::next(),
            }),
            field: "len".to_string(),
            id: NodeId::next(),
        };
        let res = fold_builtin_call(&len_call, &[Expr::string("hello world")]);
        assert_eq!(res, Some(Expr::int(11)));
    }
}
