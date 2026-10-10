//! Specialization of generalized local `val`s, and the defaulting of the remaining free type
//! variables, run on each lowered function body.
//!
//! The checker generalizes local `val`s (`val e = emptyList()` has `∀a. List<a>`), but the IR
//! types every node with one type. Each generalized `val` is therefore specialized to the
//! instances its uses actually need: an effect-free value is copied once per instance; an
//! effectful one is never copied (it would run twice), so it is specialized only when it has a
//! single instance. The pass is bottom-up (see `specialize_let`), so no use is read before the
//! `val`s it sits in have their final types.

use std::collections::BTreeSet;

use crate::types::effects::RowVarId;
use crate::types::ty::Type;

use super::ir::*;
use super::lower::{match_scheme, type_vars};

pub(super) struct Specializer<'a> {
    /// Type variables of the function's scheme.
    pub generics: &'a [usize],
    /// Row variables of the function's signature: effects chosen by the caller.
    pub effect_rows: BTreeSet<RowVarId>,
    pub next_local: LocalId,
}

impl Specializer<'_> {
    pub fn run(&mut self, body: &mut Expr) {
        let mut live = self.generics.to_vec();
        self.walk(body, &mut live);
    }

    /// `live` holds the variables that must survive: the function's generics and the variables
    /// of the enclosing `val`s that were kept generalized.
    fn walk(&mut self, expr: &mut Expr, live: &mut Vec<usize>) {
        if matches!(expr.kind, ExprKind::Let { .. }) {
            self.specialize_let(expr, live);
            return;
        }
        for child in children_mut(expr) {
            self.walk(child, live);
        }
    }

    /// Bottom-up: the body is specialized first, so the later `val`s it declares already have
    /// their final types when this `val`'s uses inside their values are read. Each copy of the
    /// value is specialized after substitution, which does the same for the `val`s it uses.
    fn specialize_let(&mut self, expr: &mut Expr, live: &mut Vec<usize>) {
        let ExprKind::Let {
            local, value, body, ..
        } = &mut expr.kind
        else {
            return;
        };
        self.walk(body, live);
        let mut own = Vec::new();
        type_vars(&value.ty, &mut own);
        own.retain(|var| !live.contains(var));
        if own.is_empty() {
            self.walk(value, live);
            return;
        }
        let mut uses = Vec::new();
        local_uses(body, *local, &mut uses);
        let mut instances: Vec<Vec<Type>> = Vec::new();
        for use_ty in &uses {
            let instance = instance_at(&own, &value.ty, use_ty, live);
            if !instances.contains(&instance) {
                instances.push(instance);
            }
        }
        match instances.as_slice() {
            [] => self.keep_generalized(value, &own, live),
            [instance] => {
                substitute(value, &own, instance);
                self.walk(value, live);
            }
            _ if !effect_free(value, &self.effect_rows) => {
                // Copying would run the effects once per copy, so the value stays generalized
                // and its uses disagree with it: the validator rejects the program (E9001).
                self.keep_generalized(value, &own, live);
            }
            _ => {
                let ty = expr.ty.clone();
                let ExprKind::Let {
                    local,
                    name,
                    value,
                    mut body,
                } = std::mem::replace(&mut expr.kind, ExprKind::Ui)
                else {
                    return;
                };
                let mut copies = Vec::new();
                for (index, instance) in instances.iter().enumerate() {
                    let mut copy = (*value).clone();
                    substitute(&mut copy, &own, instance);
                    let id = if index == 0 {
                        local
                    } else {
                        self.rename_binders(&mut copy);
                        self.fresh()
                    };
                    self.walk(&mut copy, live);
                    copies.push((id, copy));
                }
                let ids: Vec<LocalId> = copies.iter().map(|(id, _)| *id).collect();
                retarget_uses(&mut body, local, &|use_ty| {
                    let instance = instance_at(&own, &value.ty, use_ty, live);
                    instances
                        .iter()
                        .position(|other| *other == instance)
                        .map_or(local, |index| ids[index])
                });
                let mut chain = *body;
                for (id, copy) in copies.into_iter().rev() {
                    chain = Expr {
                        ty: ty.clone(),
                        kind: ExprKind::Let {
                            local: id,
                            name: name.clone(),
                            value: Box::new(copy),
                            body: Box::new(chain),
                        },
                    };
                }
                *expr = chain;
            }
        }
    }

    /// An unused `val` (its variables default to `Unit` at the end) or an effectful one used at
    /// several types: the value is specialized with its own variables kept.
    fn keep_generalized(&mut self, value: &mut Expr, own: &[usize], live: &mut Vec<usize>) {
        let mark = live.len();
        live.extend_from_slice(own);
        self.walk(value, live);
        live.truncate(mark);
    }

    fn fresh(&mut self) -> LocalId {
        let local = self.next_local;
        self.next_local += 1;
        local
    }

    /// Gives every binder inside a copied value a fresh id, so copies never share locals.
    fn rename_binders(&mut self, expr: &mut Expr) {
        let mut binders = Vec::new();
        collect_binders(expr, &mut binders);
        let renaming: Vec<(LocalId, LocalId)> =
            binders.into_iter().map(|old| (old, self.fresh())).collect();
        apply_renaming(expr, &renaming);
    }
}

/// The types the `val`'s own variables take at one use, with every variable that is not live
/// (an unconstrained one) defaulted to `Unit`.
fn instance_at(own: &[usize], value_ty: &Type, use_ty: &Type, live: &[usize]) -> Vec<Type> {
    let mut bound = std::collections::BTreeMap::new();
    match_scheme(value_ty, use_ty, &mut bound);
    own.iter()
        .map(|var| {
            bound
                .get(var)
                .map_or_else(|| Type::con("Unit"), |ty| default_vars(ty, live))
        })
        .collect()
}

fn substitute(expr: &mut Expr, own: &[usize], instance: &[Type]) {
    map_types(expr, &|ty| replace_vars(ty, own, instance));
}

fn replace_vars(ty: &Type, own: &[usize], instance: &[Type]) -> Type {
    match ty {
        Type::Var(id) => own
            .iter()
            .position(|var| var == id)
            .and_then(|index| instance.get(index).cloned())
            .unwrap_or(Type::Var(*id)),
        Type::Con(name, args) => Type::Con(
            name.clone(),
            args.iter()
                .map(|arg| replace_vars(arg, own, instance))
                .collect(),
        ),
        Type::Fun(params, result, row) => Type::Fun(
            params
                .iter()
                .map(|param| replace_vars(param, own, instance))
                .collect(),
            Box::new(replace_vars(result, own, instance)),
            row.clone(),
        ),
        Type::Error => Type::Error,
    }
}

/// Replaces every variable outside `live` by `Unit` (an unconstrained variable in monomorphic
/// code: the value is never inspected).
pub(super) fn default_vars(ty: &Type, live: &[usize]) -> Type {
    match ty {
        Type::Var(id) if live.contains(id) => Type::Var(*id),
        Type::Var(_) => Type::con("Unit"),
        Type::Con(name, args) => Type::Con(
            name.clone(),
            args.iter().map(|arg| default_vars(arg, live)).collect(),
        ),
        Type::Fun(params, result, row) => Type::Fun(
            params
                .iter()
                .map(|param| default_vars(param, live))
                .collect(),
            Box::new(default_vars(result, live)),
            row.clone(),
        ),
        Type::Error => Type::Error,
    }
}

/// Whether evaluating `expr` performs no effect. A lambda's body only runs when it is called;
/// a call is effectful when its callee's row has a label or a row variable of the signature
/// (an effect the caller chooses). `return` counts as an effect: it changes control flow.
fn effect_free(expr: &Expr, effect_rows: &BTreeSet<RowVarId>) -> bool {
    match &expr.kind {
        ExprKind::Db { .. } | ExprKind::ServerCall { .. } | ExprKind::Return(_) => false,
        ExprKind::Lambda { .. } => true,
        ExprKind::Call { callee, .. } => {
            let pure_callee = match &callee.ty {
                Type::Fun(_, _, row) => {
                    row.labels.is_empty()
                        && !row.tail.is_some_and(|tail| effect_rows.contains(&tail))
                }
                _ => false,
            };
            pure_callee
                && children(expr)
                    .iter()
                    .all(|child| effect_free(child, effect_rows))
        }
        _ => children(expr)
            .iter()
            .all(|child| effect_free(child, effect_rows)),
    }
}

fn local_uses(expr: &Expr, local: LocalId, output: &mut Vec<Type>) {
    if matches!(expr.kind, ExprKind::Local(id) if id == local) {
        output.push(expr.ty.clone());
    }
    for child in children(expr) {
        local_uses(child, local, output);
    }
}

fn retarget_uses(expr: &mut Expr, local: LocalId, target: &dyn Fn(&Type) -> LocalId) {
    if matches!(expr.kind, ExprKind::Local(id) if id == local) {
        expr.kind = ExprKind::Local(target(&expr.ty));
        return;
    }
    for child in children_mut(expr) {
        retarget_uses(child, local, target);
    }
}

fn collect_binders(expr: &Expr, output: &mut Vec<LocalId>) {
    match &expr.kind {
        ExprKind::Lambda { params, .. } => output.extend(params.iter().map(|(id, _, _)| *id)),
        ExprKind::Let { local, .. } => output.push(*local),
        ExprKind::Match { arms, .. } => {
            for arm in arms {
                output.extend(arm.bindings.iter().map(|(id, _, _)| *id));
            }
        }
        _ => {}
    }
    for child in children(expr) {
        collect_binders(child, output);
    }
}

fn apply_renaming(expr: &mut Expr, renaming: &[(LocalId, LocalId)]) {
    let rename = |id: &mut LocalId| {
        if let Some((_, new)) = renaming.iter().find(|(old, _)| old == id) {
            *id = *new;
        }
    };
    match &mut expr.kind {
        ExprKind::Local(id) => rename(id),
        ExprKind::Lambda { params, .. } => params.iter_mut().for_each(|(id, _, _)| rename(id)),
        ExprKind::Let { local, .. } => rename(local),
        ExprKind::Match { arms, .. } => {
            for arm in arms {
                arm.bindings.iter_mut().for_each(|(id, _, _)| rename(id));
            }
        }
        _ => {}
    }
    for child in children_mut(expr) {
        apply_renaming(child, renaming);
    }
}

/// Applies `f` to every type stored in the tree.
pub(super) fn map_types(expr: &mut Expr, f: &dyn Fn(&Type) -> Type) {
    expr.ty = f(&expr.ty);
    match &mut expr.kind {
        ExprKind::Global { instance, .. } | ExprKind::Builtin { instance, .. } => {
            instance.iter_mut().for_each(|ty| *ty = f(ty));
        }
        ExprKind::Lambda { params, .. } => params.iter_mut().for_each(|(_, _, ty)| *ty = f(ty)),
        ExprKind::Match { arms, .. } => {
            for arm in arms {
                arm.bindings.iter_mut().for_each(|(_, _, ty)| *ty = f(ty));
            }
        }
        _ => {}
    }
    for child in children_mut(expr) {
        map_types(child, f);
    }
}

fn children(expr: &Expr) -> Vec<&Expr> {
    match &expr.kind {
        ExprKind::Lit(_)
        | ExprKind::Local(_)
        | ExprKind::Global { .. }
        | ExprKind::Builtin { .. }
        | ExprKind::Environment(_)
        | ExprKind::None
        | ExprKind::Ui => Vec::new(),
        ExprKind::Ctor { args, .. }
        | ExprKind::ServerCall { args, .. }
        | ExprKind::Prim { args, .. }
        | ExprKind::Db { args, .. } => args.iter().collect(),
        ExprKind::Some(inner) | ExprKind::Return(inner) => vec![inner],
        ExprKind::Field { target, .. } => vec![target],
        ExprKind::Lambda { body, .. } => vec![body],
        ExprKind::Call { callee, args } => std::iter::once(&**callee).chain(args).collect(),
        ExprKind::Let { value, body, .. } | ExprKind::Seq { value, body } => vec![value, body],
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => vec![condition, then_branch, else_branch],
        ExprKind::Match { scrutinee, arms } => std::iter::once(&**scrutinee)
            .chain(arms.iter().map(|arm| &arm.body))
            .collect(),
    }
}

fn children_mut(expr: &mut Expr) -> Vec<&mut Expr> {
    match &mut expr.kind {
        ExprKind::Lit(_)
        | ExprKind::Local(_)
        | ExprKind::Global { .. }
        | ExprKind::Builtin { .. }
        | ExprKind::Environment(_)
        | ExprKind::None
        | ExprKind::Ui => Vec::new(),
        ExprKind::Ctor { args, .. }
        | ExprKind::ServerCall { args, .. }
        | ExprKind::Prim { args, .. }
        | ExprKind::Db { args, .. } => args.iter_mut().collect(),
        ExprKind::Some(inner) | ExprKind::Return(inner) => vec![inner],
        ExprKind::Field { target, .. } => vec![target],
        ExprKind::Lambda { body, .. } => vec![body],
        ExprKind::Call { callee, args } => std::iter::once(&mut **callee)
            .chain(args.iter_mut())
            .collect(),
        ExprKind::Let { value, body, .. } | ExprKind::Seq { value, body } => vec![value, body],
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => vec![condition, then_branch, else_branch],
        ExprKind::Match { scrutinee, arms } => std::iter::once(&mut **scrutinee)
            .chain(arms.iter_mut().map(|arm| &mut arm.body))
            .collect(),
    }
}
