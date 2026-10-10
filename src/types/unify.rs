//! Unification with occurs check, level adjustment and constraint propagation.

// `Type::Fun` carries an effect `Row`, so `UnifyError::Mismatch` (two types) exceeds the
// 128-byte `result_large_err` threshold. Errors are rare and cold; boxing would change the
// public shape of `UnifyError` that diagnostics and tests match on.
#![allow(clippy::result_large_err)]

use std::collections::BTreeSet;

use super::effects::{Label, Row, RowState, RowVarId, intersect};
use super::ty::{Constraint, Type, TypeTable, TypeVarId, VarState};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnifyError {
    Mismatch {
        expected: Type,
        found: Type,
    },
    Infinite {
        var: TypeVarId,
        ty: Type,
    },
    Constraint {
        constraint: Constraint,
        ty: Type,
    },
    RowMismatch {
        expected: Row,
        found: Row,
    },
    Ceiling {
        label: Label,
        ceiling: BTreeSet<Label>,
    },
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
        (Type::Fun(p1, r1, e1), Type::Fun(p2, r2, e2)) if p1.len() == p2.len() => {
            for (x, y) in p1.iter().zip(p2) {
                unify(table, x, y).map_err(|error| widen(table, error, &a, &b))?;
            }
            unify(table, r1, r2).map_err(|error| widen(table, error, &a, &b))?;
            unify_rows(table, e1, e2).map_err(|error| widen(table, error, &a, &b))
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
        UnifyError::Mismatch { .. } | UnifyError::RowMismatch { .. } => UnifyError::Mismatch {
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
        Type::Fun(params, result, row) => {
            params
                .iter()
                .try_for_each(|param| occurs_and_adjust(table, var, level, param))?;
            occurs_and_adjust(table, var, level, &result)?;
            adjust_row_level(table, &row, level);
            Ok(())
        }
        Type::Error => Ok(()),
    }
}

fn adjust_row_level(table: &mut TypeTable, row: &Row, level: u32) {
    if let Some(tail) = table.resolve_row(row).tail
        && let RowState::Unbound {
            level: tail_level,
            ceiling,
        } = table.row_state(tail).clone()
        && tail_level > level
    {
        table.set_row(tail, RowState::Unbound { level, ceiling });
    }
}

/// Unifies two effect rows (labels are distinct and unordered). Open rows exchange the labels
/// each lacks through a shared fresh tail; a closed row only equals the same label set.
pub fn unify_rows(table: &mut TypeTable, expected: &Row, found: &Row) -> Result<(), UnifyError> {
    let a = table.resolve_row(expected);
    let b = table.resolve_row(found);
    let only_a: BTreeSet<Label> = a.labels.difference(&b.labels).cloned().collect();
    let only_b: BTreeSet<Label> = b.labels.difference(&a.labels).cloned().collect();
    let mismatch = || UnifyError::RowMismatch {
        expected: a.clone(),
        found: b.clone(),
    };
    match (a.tail, b.tail) {
        (None, None) => {
            if only_a.is_empty() && only_b.is_empty() {
                Ok(())
            } else {
                Err(mismatch())
            }
        }
        (Some(x), None) => {
            if !only_a.is_empty() {
                return Err(mismatch());
            }
            bind_row(table, x, Row::closed(only_b))
        }
        (None, Some(y)) => {
            if !only_b.is_empty() {
                return Err(mismatch());
            }
            bind_row(table, y, Row::closed(only_a))
        }
        (Some(x), Some(y)) if x == y => {
            if only_a.is_empty() && only_b.is_empty() {
                Ok(())
            } else {
                Err(mismatch())
            }
        }
        (Some(x), Some(y)) => {
            let (x_level, x_ceiling) = unbound_row(table, x);
            let (y_level, y_ceiling) = unbound_row(table, y);
            // Check both ceilings before touching the table, so a failure leaves no
            // partial state behind.
            check_ceiling(&only_b, x_ceiling.as_ref())?;
            check_ceiling(&only_a, y_ceiling.as_ref())?;
            let fresh =
                table.fresh_row_var(x_level.min(y_level), intersect(&x_ceiling, &y_ceiling));
            bind_row(table, x, Row::with_tail(only_b, fresh))?;
            bind_row(table, y, Row::with_tail(only_a, fresh))
        }
    }
}

fn unbound_row(table: &TypeTable, id: RowVarId) -> (u32, Option<BTreeSet<Label>>) {
    match table.row_state(id) {
        RowState::Unbound { level, ceiling } => (*level, ceiling.clone()),
        RowState::Link(_) => unreachable!("`resolve_row` never returns a linked tail"),
    }
}

/// Fails when `labels` holds a non-rigid label outside `ceiling` (rigid labels are exempt).
fn check_ceiling(
    labels: &BTreeSet<Label>,
    ceiling: Option<&BTreeSet<Label>>,
) -> Result<(), UnifyError> {
    if let Some(ceiling) = ceiling
        && let Some(label) = labels
            .iter()
            .find(|label| !label.is_rigid() && !ceiling.contains(label))
    {
        return Err(UnifyError::Ceiling {
            label: label.clone(),
            ceiling: ceiling.clone(),
        });
    }
    Ok(())
}

/// Links a row variable, enforcing its ceiling (rigid labels are exempt) and handing the
/// ceiling and level down to the new tail.
fn bind_row(table: &mut TypeTable, var: RowVarId, row: Row) -> Result<(), UnifyError> {
    let (level, ceiling) = unbound_row(table, var);
    check_ceiling(&row.labels, ceiling.as_ref())?;
    if let Some(tail) = row.tail {
        let (tail_level, tail_ceiling) = unbound_row(table, tail);
        table.set_row(
            tail,
            RowState::Unbound {
                level: tail_level.min(level),
                ceiling: intersect(&tail_ceiling, &ceiling),
            },
        );
    }
    table.set_row(var, RowState::Link(row));
    Ok(())
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

    use crate::types::effects::{Label, Row};

    fn read() -> Label {
        Label::new("db", "read")
    }

    fn write() -> Label {
        Label::new("db", "write")
    }

    #[test]
    fn open_rows_unify_by_exchanging_missing_labels() {
        let mut table = TypeTable::default();
        let x = table.fresh_row_var(1, None);
        let y = table.fresh_row_var(1, None);
        let a = Row::with_tail([read()], x);
        let b = Row::with_tail([write()], y);
        unify_rows(&mut table, &a, &b).unwrap();
        let resolved_a = table.resolve_row(&a);
        let resolved_b = table.resolve_row(&b);
        assert_eq!(resolved_a.labels, [read(), write()].into());
        assert_eq!(resolved_a, resolved_b);
        assert!(resolved_a.tail.is_some());
    }

    #[test]
    fn closed_rows_unify_only_when_equal() {
        let mut table = TypeTable::default();
        assert!(unify_rows(&mut table, &Row::closed([read()]), &Row::closed([read()])).is_ok());
        assert!(matches!(
            unify_rows(&mut table, &Row::closed([read()]), &Row::closed([write()])),
            Err(UnifyError::RowMismatch { .. })
        ));
        let x = table.fresh_row_var(1, None);
        unify_rows(
            &mut table,
            &Row::with_tail([read()], x),
            &Row::closed([read(), write()]),
        )
        .unwrap();
        assert_eq!(
            table.resolve_row(&Row::with_tail([], x)),
            Row::closed([write()])
        );
        let y = table.fresh_row_var(1, None);
        assert!(unify_rows(&mut table, &Row::with_tail([read()], y), &Row::empty()).is_err());
    }

    #[test]
    fn same_tail_with_different_labels_is_a_mismatch() {
        let mut table = TypeTable::default();
        let x = table.fresh_row_var(1, None);
        assert!(
            unify_rows(
                &mut table,
                &Row::with_tail([read()], x),
                &Row::with_tail([], x)
            )
            .is_err()
        );
    }

    #[test]
    fn open_open_ceiling_failure_leaves_no_partial_state() {
        let mut table = TypeTable::default();
        let x = table.fresh_row_var(1, None);
        let y = table.fresh_row_var(1, Some([Label::new("server", "call")].into()));
        let error = unify_rows(
            &mut table,
            &Row::with_tail([read()], x),
            &Row::with_tail([], y),
        )
        .unwrap_err();
        assert_eq!(
            error,
            UnifyError::Ceiling {
                label: read(),
                ceiling: [Label::new("server", "call")].into()
            }
        );
        assert_eq!(
            table.resolve_row(&Row::with_tail([], x)),
            Row::with_tail([], x)
        );
        assert!(matches!(
            table.row_state(x),
            RowState::Unbound { ceiling: None, .. }
        ));
    }

    #[test]
    fn ceilings_reject_labels_and_propagate_by_intersection() {
        let mut table = TypeTable::default();
        let client = table.fresh_row_var(1, Some([Label::new("server", "call")].into()));
        let error = unify_rows(
            &mut table,
            &Row::with_tail([], client),
            &Row::closed([read()]),
        )
        .unwrap_err();
        assert_eq!(
            error,
            UnifyError::Ceiling {
                label: read(),
                ceiling: [Label::new("server", "call")].into()
            }
        );
        let server = table.fresh_row_var(1, Some([read(), write()].into()));
        let free = table.fresh_row_var(1, None);
        unify_rows(
            &mut table,
            &Row::with_tail([], server),
            &Row::with_tail([], free),
        )
        .unwrap();
        assert!(
            unify_rows(
                &mut table,
                &Row::with_tail([], free),
                &Row::closed([Label::new("server", "call")])
            )
            .is_err()
        );
        let rigid = table.fresh_row_var(1, Some(BTreeSet::new()));
        assert!(
            unify_rows(
                &mut table,
                &Row::with_tail([], rigid),
                &Row::closed([Label::rigid("e")])
            )
            .is_ok()
        );
    }

    #[test]
    fn function_types_unify_their_rows() {
        let mut table = TypeTable::default();
        let x = table.fresh_row_var(1, None);
        let a = Type::Fun(vec![int()], Box::new(int()), Row::with_tail([], x));
        let b = Type::Fun(vec![int()], Box::new(int()), Row::closed([read()]));
        unify(&mut table, &a, &b).unwrap();
        assert_eq!(display(&table.resolve(&a)), "(Int) -> Int ! {db.read}");
        let pure = Type::Fun(vec![int()], Box::new(int()), Row::empty());
        assert!(matches!(
            unify(&mut table, &pure, &b),
            Err(UnifyError::Mismatch { .. })
        ));
    }

    #[test]
    fn row_variables_are_generalized_and_instantiated() {
        let mut table = TypeTable::default();
        let a = table.fresh(2);
        let e = table.fresh_row_var(2, None);
        let inner = Type::Fun(vec![a.clone()], Box::new(a.clone()), Row::with_tail([], e));
        let ty = Type::Fun(
            vec![inner, a],
            Box::new(Type::con("Int")),
            Row::with_tail([], e),
        );
        let (scheme, _) = generalize(&table, 1, &ty);
        assert_eq!(scheme.row_vars.len(), 1);
        let first = instantiate(&mut table, &scheme, 1);
        let second = instantiate(&mut table, &scheme, 1);
        assert_ne!(first, second);
        assert_eq!(display(&first), "(('a) -> 'a ! 'e, 'a) -> Int ! 'e");
    }

    #[test]
    fn renders_rows_and_skips_e_for_type_variables() {
        let read_row = Row::closed([read()]);
        let pure = Type::Fun(vec![int()], Box::new(int()), Row::empty());
        assert_eq!(display(&pure), "(Int) -> Int");
        let reads = Type::Fun(vec![], Box::new(int()), read_row);
        assert_eq!(display(&reads), "() -> Int ! {db.read}");
        let mut table = TypeTable::default();
        let e = table.fresh_row_var(1, None);
        let both = Type::Fun(
            vec![],
            Box::new(int()),
            Row::with_tail([read(), write()], e),
        );
        // A lone tail in a positive position constrains nothing visible and is hidden; a tail
        // shared with a parameter is shown.
        assert_eq!(display(&both), "() -> Int ! {db.read, db.write}");
        let callback = Type::Fun(vec![], Box::new(int()), Row::with_tail([], e));
        let shared = Type::Fun(
            vec![callback],
            Box::new(int()),
            Row::with_tail([read(), write()], e),
        );
        assert_eq!(
            display(&shared),
            "(() -> Int ! 'e) -> Int ! {db.read, db.write | 'e}"
        );
        let vars: Vec<Type> = (0..5).map(Type::Var).collect();
        let five = Type::Fun(vars, Box::new(int()), Row::empty());
        assert_eq!(display(&five), "('a, 'b, 'c, 'd, 'f) -> Int");
        let curried = Type::Fun(vec![int()], Box::new(reads.clone()), Row::empty());
        assert_eq!(display(&curried), "(Int) -> (() -> Int ! {db.read})");
        let optional = Type::option(reads);
        assert_eq!(display(&optional), "(() -> Int ! {db.read})?");
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
        let function = Type::Fun(vec![int()], Box::new(int()), Row::empty());
        assert!(constrain(&mut table, Constraint::Eq, &function).is_err());
        assert!(constrain(&mut table, Constraint::Eq, &Type::list(function)).is_err());
        assert!(constrain(&mut table, Constraint::Eq, &Type::con("'T")).is_err());
        let a = table.fresh(1);
        constrain(&mut table, Constraint::Eq, &Type::list(a.clone())).unwrap();
        assert!(
            unify(
                &mut table,
                &a,
                &Type::Fun(vec![], Box::new(int()), Row::empty())
            )
            .is_err()
        );
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
        let ty = Type::Fun(vec![a.clone()], Box::new(a), Row::empty());
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
        let ty = Type::Fun(
            vec![Type::Var(7), Type::Var(3)],
            Box::new(Type::Var(7)),
            Row::empty(),
        );
        assert_eq!(display(&ty), "('a, 'b) -> 'a");
        assert_eq!(display(&Type::option(Type::option(int()))), "Int??");
        assert_eq!(
            display(&Type::option(Type::Fun(
                vec![],
                Box::new(int()),
                Row::empty()
            ))),
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
