//! Generic type variables, unification, substitution, and call-site instantiation.

use std::collections::HashMap;

use super::types::NeyukiType;

/// Mapping from generic type parameter names to their resolved concrete types.
pub type Substitution = HashMap<String, NeyukiType>;

/// Returns `true` if `var` occurs free anywhere inside `ty` (occurs check).
pub fn occurs_in(var: &str, ty: &NeyukiType) -> bool {
    match ty {
        NeyukiType::TypeVar(name) => name == var,
        NeyukiType::Array(inner) | NeyukiType::Optional(inner) => occurs_in(var, inner),
        NeyukiType::TableMap { key, val } => occurs_in(var, key) || occurs_in(var, val),
        NeyukiType::Record(fields) => fields.iter().any(|(_, t)| occurs_in(var, t)),
        NeyukiType::Function {
            params,
            return_type,
            ..
        } => params.iter().any(|p| occurs_in(var, p)) || occurs_in(var, return_type),
        NeyukiType::Union(variants) => variants.iter().any(|v| occurs_in(var, v)),
        NeyukiType::Generic { params, inner } => {
            !params.iter().any(|p| p == var) && occurs_in(var, inner)
        }
        _ => false,
    }
}

/// Returns `true` if `ty` contains any free type variable.
pub fn has_type_vars(ty: &NeyukiType) -> bool {
    match ty {
        NeyukiType::TypeVar(_) => true,
        NeyukiType::Array(inner) | NeyukiType::Optional(inner) => has_type_vars(inner),
        NeyukiType::TableMap { key, val } => has_type_vars(key) || has_type_vars(val),
        NeyukiType::Record(fields) => fields.iter().any(|(_, t)| has_type_vars(t)),
        NeyukiType::Function {
            params,
            return_type,
            ..
        } => params.iter().any(has_type_vars) || has_type_vars(return_type),
        NeyukiType::Union(variants) => variants.iter().any(has_type_vars),
        NeyukiType::Generic { params, inner } => {
            let mut free = Vec::new();
            collect_type_vars(inner, &mut free);
            free.iter().any(|v| !params.contains(v))
        }
        _ => false,
    }
}

/// Collects free type-variable names in traversal order without duplicates.
pub fn collect_type_vars(ty: &NeyukiType, out: &mut Vec<String>) {
    match ty {
        NeyukiType::TypeVar(name) => {
            if !out.contains(name) {
                out.push(name.clone());
            }
        }
        NeyukiType::Array(inner) | NeyukiType::Optional(inner) => collect_type_vars(inner, out),
        NeyukiType::TableMap { key, val } => {
            collect_type_vars(key, out);
            collect_type_vars(val, out);
        }
        NeyukiType::Record(fields) => {
            for (_, field_ty) in fields {
                collect_type_vars(field_ty, out);
            }
        }
        NeyukiType::Function {
            params,
            return_type,
            ..
        } => {
            for p in params {
                collect_type_vars(p, out);
            }
            collect_type_vars(return_type, out);
        }
        NeyukiType::Union(variants) => {
            for v in variants {
                collect_type_vars(v, out);
            }
        }
        NeyukiType::Generic { params, inner } => {
            let mut nested = Vec::new();
            collect_type_vars(inner, &mut nested);
            for v in nested {
                if !params.contains(&v) && !out.contains(&v) {
                    out.push(v);
                }
            }
        }
        _ => {}
    }
}

/// Applies `subst` capture-avoidingly to every free `TypeVar` in `ty`.
pub fn substitute(ty: &NeyukiType, subst: &Substitution) -> NeyukiType {
    if subst.is_empty() {
        return ty.clone();
    }
    match ty {
        NeyukiType::TypeVar(name) => subst
            .get(name)
            .cloned()
            .unwrap_or_else(|| NeyukiType::TypeVar(name.clone())),
        NeyukiType::Array(inner) => NeyukiType::Array(Box::new(substitute(inner, subst))),
        NeyukiType::Optional(inner) => NeyukiType::Optional(Box::new(substitute(inner, subst))),
        NeyukiType::TableMap { key, val } => NeyukiType::TableMap {
            key: Box::new(substitute(key, subst)),
            val: Box::new(substitute(val, subst)),
        },
        NeyukiType::Record(fields) => NeyukiType::Record(
            fields
                .iter()
                .map(|(k, v)| (k.clone(), substitute(v, subst)))
                .collect(),
        ),
        NeyukiType::Function {
            params,
            return_type,
            is_vararg,
        } => NeyukiType::Function {
            params: params.iter().map(|p| substitute(p, subst)).collect(),
            return_type: Box::new(substitute(return_type, subst)),
            is_vararg: *is_vararg,
        },
        NeyukiType::Union(variants) => {
            NeyukiType::Union(variants.iter().map(|v| substitute(v, subst)).collect())
        }
        NeyukiType::Generic { params, inner } => {
            let mut filtered = subst.clone();
            for p in params {
                filtered.remove(p);
            }
            NeyukiType::Generic {
                params: params.clone(),
                inner: Box::new(substitute(inner, &filtered)),
            }
        }
        other => other.clone(),
    }
}

/// Unifies a formal `pattern` type (which may contain `TypeVar`s) against an `actual` type.
pub fn unify(
    pattern: &NeyukiType,
    actual: &NeyukiType,
    subst: &mut Substitution,
) -> Result<(), String> {
    if pattern == actual || *pattern == NeyukiType::Any || *actual == NeyukiType::Never {
        return Ok(());
    }

    if let NeyukiType::TypeVar(var) = pattern {
        return unify_var(var, actual, subst);
    }

    if *actual == NeyukiType::Any {
        // Dynamic arguments unify trivially with concrete or compound patterns.
        return Ok(());
    }

    match (pattern, actual) {
        (NeyukiType::Optional(p), NeyukiType::Optional(a)) => unify(p, a, subst),
        (NeyukiType::Optional(_), NeyukiType::Nil) => Ok(()),
        (NeyukiType::Optional(p), a) => unify(p, a, subst),
        (NeyukiType::Array(p), NeyukiType::Array(a)) => unify(p, a, subst),
        (NeyukiType::TableMap { key: pk, val: pv }, NeyukiType::TableMap { key: ak, val: av }) => {
            unify(pk, ak, subst)?;
            unify(pv, av, subst)
        }
        (NeyukiType::Record(pf), NeyukiType::Record(af)) => {
            for (k, p_ty) in pf {
                let Some((_, a_ty)) = af.iter().find(|(name, _)| name == k) else {
                    return Err(format!(
                        "missing record field '{}' required by '{}'",
                        k,
                        pattern.display_name()
                    ));
                };
                unify(p_ty, a_ty, subst)?;
            }
            Ok(())
        }
        (
            NeyukiType::Function {
                params: pp,
                return_type: pr,
                is_vararg: pv,
            },
            NeyukiType::Function {
                params: ap,
                return_type: ar,
                is_vararg: av,
            },
        ) => {
            let arity_ok = if *pv {
                pp.len() <= ap.len() || *av
            } else {
                !*av && pp.len() == ap.len()
            };
            if !arity_ok {
                return Err(format!(
                    "function arity mismatch: expected {}, got {}",
                    pp.len(),
                    ap.len()
                ));
            }
            for (p_arg, a_arg) in pp.iter().zip(ap.iter()) {
                if has_type_vars(p_arg) {
                    unify(p_arg, a_arg, subst)?;
                } else if !p_arg.is_assignable_to(a_arg) {
                    return Err(format!(
                        "incompatible function parameter: expected '{}', got '{}'",
                        p_arg.display_name(),
                        a_arg.display_name()
                    ));
                }
            }
            unify(pr, ar, subst)
        }
        (NeyukiType::Union(variants), _) if has_type_vars(pattern) => {
            for variant in variants {
                let mut candidate = subst.clone();
                if unify(variant, actual, &mut candidate).is_ok() {
                    *subst = candidate;
                    return Ok(());
                }
            }
            Err(format!(
                "expected '{}', got '{}'",
                pattern.display_name(),
                actual.display_name()
            ))
        }
        _ => {
            if actual.is_assignable_to(pattern) {
                Ok(())
            } else {
                Err(format!(
                    "expected '{}', got '{}'",
                    pattern.display_name(),
                    actual.display_name()
                ))
            }
        }
    }
}

fn unify_var(var: &str, actual: &NeyukiType, subst: &mut Substitution) -> Result<(), String> {
    if let NeyukiType::TypeVar(other) = actual
        && other == var
    {
        return Ok(());
    }

    if let Some(existing) = subst.get(var).cloned() {
        let resolved = substitute(actual, subst);
        if existing == resolved || resolved == NeyukiType::Any {
            return Ok(());
        }
        if resolved.is_assignable_to(&existing) {
            return Ok(());
        }
        if existing.is_assignable_to(&resolved) {
            if occurs_in(var, &resolved) {
                return Err(format!(
                    "infinite type: '{}' occurs in '{}'",
                    var,
                    resolved.display_name()
                ));
            }
            subst.insert(var.to_string(), resolved);
            return Ok(());
        }
        if (existing == NeyukiType::Int && resolved == NeyukiType::Float)
            || (existing == NeyukiType::Float && resolved == NeyukiType::Int)
        {
            subst.insert(var.to_string(), NeyukiType::Number);
            return Ok(());
        }
        return Err(format!(
            "cannot unify type parameter '{}' between '{}' and '{}'",
            var,
            existing.display_name(),
            resolved.display_name()
        ));
    }

    let resolved = substitute(actual, subst);
    if occurs_in(var, &resolved) {
        return Err(format!(
            "infinite type: '{}' occurs in '{}'",
            var,
            resolved.display_name()
        ));
    }
    subst.insert(var.to_string(), resolved);
    Ok(())
}

/// Instantiates a generic type with positional explicit `type_args`.
pub fn instantiate_generic(
    params: &[String],
    inner: &NeyukiType,
    type_args: &[NeyukiType],
) -> NeyukiType {
    let mut subst = Substitution::new();
    for (idx, param) in params.iter().enumerate() {
        let arg = type_args.get(idx).cloned().unwrap_or(NeyukiType::Any);
        subst.insert(param.clone(), arg);
    }
    substitute(inner, &subst)
}

/// Instantiates a callee type at a call site using inferred argument types,
/// returning `(return_type, substitution)`.
pub fn instantiate_call(
    callee_ty: &NeyukiType,
    arg_tys: &[NeyukiType],
) -> Result<(NeyukiType, Substitution), String> {
    match callee_ty {
        NeyukiType::Generic { params, inner } => match inner.as_ref() {
            NeyukiType::Function {
                params: fn_params,
                return_type,
                is_vararg,
            } => {
                if !*is_vararg && arg_tys.len() > fn_params.len() {
                    return Err(format!(
                        "too many arguments: expected at most {}, got {}",
                        fn_params.len(),
                        arg_tys.len()
                    ));
                }
                let mut subst = Substitution::new();
                for (formal, actual) in fn_params.iter().zip(arg_tys.iter()) {
                    unify(formal, actual, &mut subst)?;
                }
                for param in params {
                    subst.entry(param.clone()).or_insert(NeyukiType::Any);
                }
                Ok((substitute(return_type, &subst), subst))
            }
            _ => Ok((substitute(inner, &Substitution::new()), Substitution::new())),
        },
        NeyukiType::Function { return_type, .. } => {
            Ok(((**return_type).clone(), Substitution::new()))
        }
        _ => Ok((NeyukiType::Any, Substitution::new())),
    }
}

/// Checks whether a generic type `Generic { params, inner }` can be instantiated
/// so that its inner type matches `target`.
pub fn instantiate_to_target(params: &[String], inner: &NeyukiType, target: &NeyukiType) -> bool {
    let mut subst = Substitution::new();
    if unify(inner, target, &mut subst).is_err() {
        return false;
    }
    subst.keys().all(|k| params.iter().any(|p| p == k))
}

/// Wraps a function signature in `NeyukiType::Generic` when it contains free type variables.
pub fn generalize_function(
    param_tys: Vec<NeyukiType>,
    return_ty: NeyukiType,
    is_vararg: bool,
) -> NeyukiType {
    let func = NeyukiType::Function {
        params: param_tys,
        return_type: Box::new(return_ty),
        is_vararg,
    };
    let mut vars = Vec::new();
    collect_type_vars(&func, &mut vars);
    if vars.is_empty() {
        func
    } else {
        NeyukiType::Generic {
            params: vars,
            inner: Box::new(func),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generic_identity_call_instantiation() {
        let id_ty = generalize_function(
            vec![NeyukiType::TypeVar("T".to_string())],
            NeyukiType::TypeVar("T".to_string()),
            false,
        );
        assert!(matches!(id_ty, NeyukiType::Generic { .. }));

        let (ret_int, subst_int) = instantiate_call(&id_ty, &[NeyukiType::Int]).unwrap();
        assert_eq!(ret_int, NeyukiType::Int);
        assert_eq!(subst_int.get("T"), Some(&NeyukiType::Int));

        let (ret_str, _) = instantiate_call(&id_ty, &[NeyukiType::String]).unwrap();
        assert_eq!(ret_str, NeyukiType::String);
    }

    #[test]
    fn test_generic_pair_unification_and_conflict() {
        let pair_ty = generalize_function(
            vec![
                NeyukiType::TypeVar("T".to_string()),
                NeyukiType::TypeVar("T".to_string()),
            ],
            NeyukiType::Array(Box::new(NeyukiType::TypeVar("T".to_string()))),
            false,
        );

        let (ret_num, _) =
            instantiate_call(&pair_ty, &[NeyukiType::Int, NeyukiType::Float]).unwrap();
        assert_eq!(ret_num, NeyukiType::Array(Box::new(NeyukiType::Number)));

        let conflict = instantiate_call(&pair_ty, &[NeyukiType::Int, NeyukiType::String]);
        assert!(conflict.is_err());
    }

    #[test]
    fn test_occurs_check_rejects_infinite_type() {
        let mut subst = Substitution::new();
        let res = unify(
            &NeyukiType::TypeVar("T".to_string()),
            &NeyukiType::Array(Box::new(NeyukiType::TypeVar("T".to_string()))),
            &mut subst,
        );
        assert!(res.is_err());
        assert!(occurs_in(
            "T",
            &NeyukiType::Array(Box::new(NeyukiType::TypeVar("T".to_string())))
        ));
    }

    #[test]
    fn test_instantiate_generic_explicit_args_and_assignability() {
        let box_fn = generalize_function(
            vec![NeyukiType::TypeVar("T".to_string())],
            NeyukiType::Record(vec![(
                "value".to_string(),
                NeyukiType::TypeVar("T".to_string()),
            )]),
            false,
        );
        let instantiated = instantiate_generic(
            &["T".to_string()],
            &NeyukiType::Record(vec![(
                "value".to_string(),
                NeyukiType::TypeVar("T".to_string()),
            )]),
            &[NeyukiType::Boolean],
        );
        assert_eq!(
            instantiated,
            NeyukiType::Record(vec![("value".to_string(), NeyukiType::Boolean)])
        );

        let concrete_fn = NeyukiType::Function {
            params: vec![NeyukiType::Int],
            return_type: Box::new(NeyukiType::Record(vec![(
                "value".to_string(),
                NeyukiType::Int,
            )])),
            is_vararg: false,
        };
        assert!(box_fn.is_assignable_to(&concrete_fn));
    }
}
