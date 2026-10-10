//! Unification with occurs check, level adjustment and constraint propagation.

use super::ty::{Constraint, Type, TypeTable, TypeVarId, VarState};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnifyError {
    Mismatch { expected: Type, found: Type },
    Infinite { var: TypeVarId, ty: Type },
    Constraint { constraint: Constraint, ty: Type },
}

pub fn unify(table: &mut TypeTable, expected: &Type, found: &Type) -> Result<(), UnifyError> {
    let a = table.shallow(expected);
    let b = table.shallow(found);
    match (&a, &b) {
        // An unbound variable meeting `Error` is poisoned, so its operator constraint cannot
        // resurface later as a spurious ambiguity next to the error already reported.
        (Type::Var(x), Type::Error) | (Type::Error, Type::Var(x)) => {
            table.set(*x, VarState::Link(Type::Error));
            Ok(())
        }
        (Type::Error, _) | (_, Type::Error) => Ok(()),
        (Type::Var(x), Type::Var(y)) if x == y => Ok(()),
        (Type::Var(x), _) => bind(table, *x, &b),
        (_, Type::Var(y)) => bind(table, *y, &a),
        (Type::Con(n1, a1), Type::Con(n2, a2)) if n1 == n2 && a1.len() == a2.len() => {
            for (x, y) in a1.iter().zip(a2) {
                unify(table, x, y).map_err(|error| widen(table, error, &a, &b))?;
            }
            Ok(())
        }
        (Type::Fun(p1, r1), Type::Fun(p2, r2)) if p1.len() == p2.len() => {
            for (x, y) in p1.iter().zip(p2) {
                unify(table, x, y).map_err(|error| widen(table, error, &a, &b))?;
            }
            unify(table, r1, r2).map_err(|error| widen(table, error, &a, &b))
        }
        _ => Err(UnifyError::Mismatch {
            expected: table.resolve(&a),
            found: table.resolve(&b),
        }),
    }
}

/// A nested mismatch is reported with the enclosing types, which is what the programmer wrote.
fn widen(table: &TypeTable, error: UnifyError, expected: &Type, found: &Type) -> UnifyError {
    match error {
        UnifyError::Mismatch { .. } => UnifyError::Mismatch {
            expected: table.resolve(expected),
            found: table.resolve(found),
        },
        other => other,
    }
}

fn bind(table: &mut TypeTable, var: TypeVarId, ty: &Type) -> Result<(), UnifyError> {
    let VarState::Unbound { level, constraint } = table.state(var).clone() else {
        unreachable!("`shallow` never returns a linked variable")
    };
    if let Type::Var(other) = ty {
        let VarState::Unbound {
            level: other_level,
            constraint: other_constraint,
        } = table.state(*other).clone()
        else {
            unreachable!("`shallow` never returns a linked variable")
        };
        let merged = match (constraint, other_constraint) {
            (Some(a), Some(b)) => Some(a.meet(b)),
            (a, b) => a.or(b),
        };
        table.set(
            *other,
            VarState::Unbound {
                level: level.min(other_level),
                constraint: merged,
            },
        );
        table.set(var, VarState::Link(ty.clone()));
        return Ok(());
    }
    if occurs_and_adjust(table, var, level, ty).is_err() {
        let error = UnifyError::Infinite {
            var,
            ty: table.resolve(ty),
        };
        table.set(var, VarState::Link(Type::Error));
        return Err(error);
    }
    // Any rejection (infinite type, constraint failure) links the variable to `Error`: the
    // problem is reported once, and later uses neither cascade nor surface as an ambiguous
    // (still constrained) variable.
    if let Some(constraint) = constraint
        && let Err(error) = constrain(table, constraint, ty)
    {
        table.set(var, VarState::Link(Type::Error));
        return Err(error);
    }
    table.set(var, VarState::Link(ty.clone()));
    Ok(())
}

fn occurs_and_adjust(
    table: &mut TypeTable,
    var: TypeVarId,
    level: u32,
    ty: &Type,
) -> Result<(), ()> {
    match table.shallow(ty) {
        Type::Var(id) => {
            if id == var {
                return Err(());
            }
            if let VarState::Unbound {
                level: other_level,
                constraint,
            } = table.state(id).clone()
                && other_level > level
            {
                table.set(id, VarState::Unbound { level, constraint });
            }
            Ok(())
        }
        Type::Con(_, args) => args
            .iter()
            .try_for_each(|arg| occurs_and_adjust(table, var, level, arg)),
        Type::Fun(params, result) => {
            params
                .iter()
                .try_for_each(|param| occurs_and_adjust(table, var, level, param))?;
            occurs_and_adjust(table, var, level, &result)
        }
        Type::Error => Ok(()),
    }
}

/// Requires `ty` to satisfy `constraint`; an unbound variable records it for later.
pub fn constrain(
    table: &mut TypeTable,
    constraint: Constraint,
    ty: &Type,
) -> Result<(), UnifyError> {
    let fail = |table: &TypeTable| UnifyError::Constraint {
        constraint,
        ty: table.resolve(ty),
    };
    match table.shallow(ty) {
        Type::Error => Ok(()),
        Type::Var(id) => {
            if let VarState::Unbound {
                level,
                constraint: existing,
            } = table.state(id).clone()
            {
                let merged = existing.map_or(constraint, |existing| existing.meet(constraint));
                table.set(
                    id,
                    VarState::Unbound {
                        level,
                        constraint: Some(merged),
                    },
                );
            }
            Ok(())
        }
        Type::Con(name, args) => {
            if !constraint.accepts(&name)
                || (constraint == Constraint::Eq && table.is_non_equatable(&name))
            {
                return Err(fail(table));
            }
            if constraint == Constraint::Eq {
                for arg in &args {
                    constrain(table, Constraint::Eq, arg).map_err(|_| fail(table))?;
                }
                return Ok(());
            }
            if args.is_empty() {
                Ok(())
            } else {
                Err(fail(table))
            }
        }
        Type::Fun(..) => Err(fail(table)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ty::*;

    fn int() -> Type {
        Type::con("Int")
    }

    #[test]
    fn binds_variables_and_resolves() {
        let mut table = TypeTable::default();
        let a = table.fresh(1);
        unify(&mut table, &Type::list(a.clone()), &Type::list(int())).unwrap();
        assert_eq!(table.resolve(&a), int());
    }

    #[test]
    fn reports_mismatch_with_enclosing_types() {
        let mut table = TypeTable::default();
        let error = unify(
            &mut table,
            &Type::list(int()),
            &Type::list(Type::con("String")),
        )
        .unwrap_err();
        assert_eq!(
            error,
            UnifyError::Mismatch {
                expected: Type::list(int()),
                found: Type::list(Type::con("String"))
            }
        );
    }

    #[test]
    fn occurs_check_rejects_infinite_types() {
        let mut table = TypeTable::default();
        let a = table.fresh(1);
        let error = unify(&mut table, &a, &Type::list(a.clone())).unwrap_err();
        assert!(matches!(error, UnifyError::Infinite { .. }));
        assert_eq!(table.resolve(&a), Type::Error);
    }

    #[test]
    fn unification_lowers_levels() {
        let mut table = TypeTable::default();
        let outer = table.fresh(1);
        let inner = table.fresh(3);
        unify(&mut table, &outer, &Type::list(inner.clone())).unwrap();
        let Type::Var(id) = inner else { unreachable!() };
        assert_eq!(
            table.state(id),
            &VarState::Unbound {
                level: 1,
                constraint: None
            }
        );
    }

    #[test]
    fn constraints_accept_and_reject_types() {
        let mut table = TypeTable::default();
        let a = table.fresh(1);
        constrain(&mut table, Constraint::Num, &a).unwrap();
        assert!(unify(&mut table, &a, &Type::con("String")).is_err());
        let b = table.fresh(1);
        constrain(&mut table, Constraint::Add, &b).unwrap();
        unify(&mut table, &b, &Type::con("String")).unwrap();
        assert!(constrain(&mut table, Constraint::Sub, &Type::con("String")).is_err());
        assert!(constrain(&mut table, Constraint::Ord, &Type::con("Instant")).is_ok());
    }

    #[test]
    fn merging_variables_keeps_the_stronger_constraint() {
        let mut table = TypeTable::default();
        let a = table.fresh(1);
        let b = table.fresh(1);
        constrain(&mut table, Constraint::Add, &a).unwrap();
        constrain(&mut table, Constraint::Num, &b).unwrap();
        unify(&mut table, &a, &b).unwrap();
        assert!(unify(&mut table, &a, &Type::con("String")).is_err());
        unify(&mut table, &a, &Type::con("Double")).unwrap();
    }

    #[test]
    fn equality_rejects_functions_and_rigid_types() {
        let mut table = TypeTable::default();
        let function = Type::Fun(vec![int()], Box::new(int()));
        assert!(constrain(&mut table, Constraint::Eq, &function).is_err());
        assert!(constrain(&mut table, Constraint::Eq, &Type::list(function)).is_err());
        assert!(constrain(&mut table, Constraint::Eq, &Type::con("'T")).is_err());
        let a = table.fresh(1);
        constrain(&mut table, Constraint::Eq, &Type::list(a.clone())).unwrap();
        assert!(unify(&mut table, &a, &Type::Fun(vec![], Box::new(int()))).is_err());
    }

    #[test]
    fn equality_rejects_non_equatable_constructors() {
        let mut table = TypeTable::default();
        table.set_non_equatable(["H".to_string(), "Ui".to_string()].into());
        assert!(constrain(&mut table, Constraint::Eq, &Type::con("H")).is_err());
        assert!(constrain(&mut table, Constraint::Eq, &Type::list(Type::con("Ui"))).is_err());
        assert!(constrain(&mut table, Constraint::Eq, &Type::con("P")).is_ok());
        let a = table.fresh(1);
        constrain(&mut table, Constraint::Eq, &a).unwrap();
        assert!(unify(&mut table, &a, &Type::con("H")).is_err());
    }

    #[test]
    fn error_type_unifies_with_everything() {
        let mut table = TypeTable::default();
        unify(&mut table, &Type::Error, &int()).unwrap();
        constrain(&mut table, Constraint::Num, &Type::Error).unwrap();
    }

    #[test]
    fn generalizes_and_instantiates_by_level() {
        let mut table = TypeTable::default();
        let a = table.fresh(2);
        let ty = Type::Fun(vec![a.clone()], Box::new(a));
        let (scheme, ambiguous) = generalize(&table, 1, &ty);
        assert!(ambiguous.is_empty());
        assert_eq!(scheme.vars.len(), 1);
        let first = instantiate(&mut table, &scheme, 1);
        let second = instantiate(&mut table, &scheme, 1);
        assert_ne!(first, second);
        assert_eq!(display(&first), "('a) -> 'a");
        let c = table.fresh(2);
        constrain(&mut table, Constraint::Num, &c).unwrap();
        let (_, ambiguous) = generalize(&table, 1, &c);
        assert_eq!(ambiguous.len(), 1);
    }

    #[test]
    fn renders_with_normalized_names() {
        let ty = Type::Fun(vec![Type::Var(7), Type::Var(3)], Box::new(Type::Var(7)));
        assert_eq!(display(&ty), "('a, 'b) -> 'a");
        assert_eq!(display(&Type::option(Type::option(int()))), "Int??");
        assert_eq!(
            display(&Type::option(Type::Fun(vec![], Box::new(int())))),
            "(() -> Int)?"
        );
        let names = display_all(&[&Type::Var(4), &Type::list(Type::Var(4))]);
        assert_eq!(names, vec!["'a".to_string(), "List<'a>".to_string()]);
    }

    #[test]
    fn error_poisons_constrained_variables() {
        for flip in [false, true] {
            let mut table = TypeTable::default();
            let a = table.fresh(1);
            constrain(&mut table, Constraint::Num, &a).unwrap();
            if flip {
                unify(&mut table, &Type::Error, &a).unwrap();
            } else {
                unify(&mut table, &a, &Type::Error).unwrap();
            }
            assert_eq!(table.resolve(&a), Type::Error);
            assert!(generalize(&table, 0, &a).1.is_empty());
        }
    }
}
