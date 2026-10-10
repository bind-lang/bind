//! Rust rendering of core IR types and of the program's type declarations.
//!
//! Every std, runtime and dependency item is written with a full path (`::std::sync::Arc`,
//! `crate::bind_runtime::Int`, ...) so user types, emitted verbatim, can never shadow them.
//! Primitive types (`bool`, `f64`, ...) use their short names unless the program declares a
//! type of the same name, in which case `::std::primitive::<name>` is used.

use std::collections::{BTreeMap, BTreeSet};

use crate::core::ir::{CoreProgram, CoreType};
use crate::types::ty::Type;

pub const ARC: &str = "::std::sync::Arc";
pub const BOX: &str = "::std::boxed::Box";
pub const OPTION: &str = "::std::option::Option";
pub const STRING: &str = "::std::string::String";
pub const RESULT: &str = "::std::result::Result";
pub const RUNTIME: &str = "crate::bind_runtime";

/// The Rust type of `ty`. Type variables render as `T<id>`; inside a function the backend
/// uses the function's positional generic names instead (see [`Layout::ty`]).
pub fn rust_type(ty: &Type, program: &CoreProgram) -> String {
    Layout::new(program).ty(ty, &BTreeMap::new())
}

/// Declarations of every program type (`Result` excluded: it is the runtime's `BindResult`).
pub fn type_declarations(program: &CoreProgram) -> String {
    super::indent(&Layout::new(program).declarations())
}

/// What a constructor builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CtorKind {
    Record,
    Variant,
    Distinct,
    /// `Ok` / `Err` of the built-in `Result` (the runtime's `BindResult`).
    BindResult,
}

/// A constructor of a program type, with its fields still written with `'P` markers.
#[derive(Clone, Debug)]
pub struct CtorDef<'p> {
    pub kind: CtorKind,
    pub type_name: &'p str,
    pub name: &'p str,
    pub params: &'p [String],
    pub fields: Vec<(&'p str, &'p Type)>,
}

impl CtorDef<'_> {
    /// Field types at a concrete instance `ty` of the constructed type.
    pub fn field_types(&self, ty: &Type) -> Vec<Type> {
        let args: &[Type] = match ty {
            Type::Con(name, args) if name == self.type_name => args,
            _ => &[],
        };
        let markers: BTreeMap<String, &Type> = self
            .params
            .iter()
            .zip(args)
            .map(|(param, arg)| (format!("'{param}"), arg))
            .collect();
        self.fields
            .iter()
            .map(|(_, field)| substitute(field, &markers))
            .collect()
    }
}

fn substitute(ty: &Type, markers: &BTreeMap<String, &Type>) -> Type {
    match ty {
        Type::Con(name, args) if args.is_empty() && name.starts_with('\'') => markers
            .get(name)
            .map_or_else(|| ty.clone(), |arg| (*arg).clone()),
        Type::Con(name, args) => Type::Con(
            name.clone(),
            args.iter().map(|arg| substitute(arg, markers)).collect(),
        ),
        Type::Fun(params, result, row) => Type::Fun(
            params
                .iter()
                .map(|param| substitute(param, markers))
                .collect(),
            Box::new(substitute(result, markers)),
            row.clone(),
        ),
        other => other.clone(),
    }
}

/// Types whose Rust representation is `Copy`: read without `.clone()`.
const COPY_SCALARS: &[&str] = &[
    "Boolean", "Double", "Float", "Long", "Port", "Unit", "Decimal", "Money", "Date", "DateTime",
    "Instant", "Duration", "UUID",
];

/// Program-wide facts the backend needs: constructors, boxed (recursive) fields and names.
pub struct Layout<'p> {
    pub program: &'p CoreProgram,
    /// `(constructor, field index)` of every field stored as `Box<T>` to keep its type sized.
    boxed: BTreeSet<(String, usize)>,
    user_types: BTreeSet<&'p str>,
}

impl<'p> Layout<'p> {
    pub fn new(program: &'p CoreProgram) -> Layout<'p> {
        let user_types = program
            .types
            .iter()
            .filter_map(|ty| match ty {
                CoreType::Sum { name, .. } if name == "Result" => None,
                CoreType::Record { name, .. }
                | CoreType::Sum { name, .. }
                | CoreType::Distinct { name, .. } => Some(name.as_str()),
            })
            .collect();
        let mut layout = Layout {
            program,
            boxed: BTreeSet::new(),
            user_types,
        };
        layout.boxed = layout.recursive_fields();
        layout
    }

    /// The constructor `ctor` of type `type_name`.
    pub fn ctor(&self, type_name: &str, ctor: &str) -> Option<CtorDef<'p>> {
        self.ctors_of(type_name)
            .into_iter()
            .find(|def| def.name == ctor)
    }

    /// The constructors of `type_name`, in declaration order.
    pub fn ctors_of(&self, type_name: &str) -> Vec<CtorDef<'p>> {
        let fields = |fields: &'p [(String, Type)]| -> Vec<(&'p str, &'p Type)> {
            fields
                .iter()
                .map(|(name, ty)| (name.as_str(), ty))
                .collect()
        };
        for ty in &self.program.types {
            match ty {
                CoreType::Record {
                    name,
                    params,
                    fields: record,
                } if name == type_name => {
                    return vec![CtorDef {
                        kind: CtorKind::Record,
                        type_name: name,
                        name,
                        params,
                        fields: fields(record),
                    }];
                }
                CoreType::Sum {
                    name,
                    params,
                    variants,
                } if name == type_name => {
                    let kind = if name == "Result" {
                        CtorKind::BindResult
                    } else {
                        CtorKind::Variant
                    };
                    return variants
                        .iter()
                        .map(|(variant, variant_fields)| CtorDef {
                            kind,
                            type_name: name,
                            name: variant,
                            params,
                            fields: fields(variant_fields),
                        })
                        .collect();
                }
                CoreType::Distinct { name, underlying } if name == type_name => {
                    return vec![CtorDef {
                        kind: CtorKind::Distinct,
                        type_name: name,
                        name,
                        params: &[],
                        fields: vec![("value", underlying)],
                    }];
                }
                _ => {}
            }
        }
        Vec::new()
    }

    pub fn is_boxed(&self, ctor: &str, index: usize) -> bool {
        self.boxed.contains(&(ctor.to_string(), index))
    }

    /// The Rust name of a primitive type, qualified when a user type shadows it.
    fn primitive(&self, name: &'static str) -> String {
        if self.user_types.contains(name) {
            format!("::std::primitive::{name}")
        } else {
            name.to_string()
        }
    }

    pub fn is_copy(&self, ty: &Type) -> bool {
        matches!(ty, Type::Con(name, args) if args.is_empty()
            && COPY_SCALARS.contains(&name.as_str())
            && !self.user_types.contains(name.as_str()))
    }

    /// The Rust type of `ty`; `generics` names the type variables of the current function.
    pub fn ty(&self, ty: &Type, generics: &BTreeMap<usize, String>) -> String {
        let all = |args: &[Type]| -> Vec<String> {
            args.iter().map(|arg| self.ty(arg, generics)).collect()
        };
        match ty {
            Type::Var(id) => generics
                .get(id)
                .cloned()
                .unwrap_or_else(|| format!("T{id}")),
            Type::Error => "::std::compile_error!(\"bindc: invalid type\")".into(),
            Type::Fun(params, result, _) => {
                let mut inputs = vec![format!("{RUNTIME}::Ctx")];
                inputs.extend(all(params));
                format!(
                    "{ARC}<dyn ::std::ops::Fn({}) -> {RUNTIME}::BoxFuture<{}> + ::std::marker::Send + ::std::marker::Sync>",
                    inputs.join(", "),
                    self.ty(result, generics)
                )
            }
            Type::Con(name, args) if args.is_empty() && name.starts_with('\'') => {
                name.trim_start_matches('\'').to_string()
            }
            Type::Con(name, args) if self.user_types.contains(name.as_str()) => {
                if args.is_empty() {
                    name.clone()
                } else {
                    format!("{name}<{}>", all(args).join(", "))
                }
            }
            Type::Con(name, args) => match (name.as_str(), all(args).as_slice()) {
                ("Int", []) => format!("{RUNTIME}::Int"),
                ("Long", []) => self.primitive("i64"),
                ("Double", []) => self.primitive("f64"),
                ("Float", []) => self.primitive("f32"),
                ("Boolean", []) => self.primitive("bool"),
                ("Port", []) => self.primitive("u16"),
                ("Unit" | "Ui", []) => "()".into(),
                ("String" | "Email" | "Url" | "DatabaseUrl" | "Secret", []) => STRING.into(),
                ("Decimal" | "Money", []) => "::rust_decimal::Decimal".into(),
                ("Date", []) => "::chrono::NaiveDate".into(),
                ("DateTime", []) => "::chrono::NaiveDateTime".into(),
                ("Instant", []) => "::chrono::DateTime<::chrono::Utc>".into(),
                ("Duration", []) => "::chrono::TimeDelta".into(),
                ("UUID", []) => "::uuid::Uuid".into(),
                ("Option", [inner]) => format!("{OPTION}<{inner}>"),
                ("List" | "Set", [inner]) => format!("{ARC}<::std::vec::Vec<{inner}>>"),
                ("Map", [key, value]) => format!("{ARC}<::std::vec::Vec<({key}, {value})>>"),
                ("Result", [ok, err]) => format!("{RUNTIME}::BindResult<{ok}, {err}>"),
                _ => "::std::compile_error!(\"bindc: unknown type\")".into(),
            },
        }
    }

    // ---- recursion -----------------------------------------------------------------------

    /// User types stored inline by a value of type `ty` (behind `Option`, `Result` and user
    /// type arguments; `List`, `Set`, `Map` and functions are behind a pointer already).
    fn inline_types(&self, ty: &Type, output: &mut BTreeSet<&'p str>) {
        let Type::Con(name, args) = ty else {
            return;
        };
        if let Some(user) = self.user_types.get(name.as_str()) {
            output.insert(user);
        } else if !matches!(name.as_str(), "Option" | "Result") {
            return;
        }
        for arg in args {
            self.inline_types(arg, output);
        }
    }

    /// Fields whose inline types reach back to the declaring type: boxing them breaks every
    /// cycle of inline storage, so each type has a finite size.
    fn recursive_fields(&self) -> BTreeSet<(String, usize)> {
        let mut edges: BTreeMap<&'p str, BTreeSet<&'p str>> = BTreeMap::new();
        for name in &self.user_types {
            let entry = edges.entry(name).or_default();
            for ctor in self.ctors_of(name) {
                for (_, field) in &ctor.fields {
                    self.inline_types(field, entry);
                }
            }
        }
        let reaches = |from: &'p str, target: &str| -> bool {
            let mut seen = BTreeSet::new();
            let mut stack = vec![from];
            while let Some(next) = stack.pop() {
                if next == target {
                    return true;
                }
                if seen.insert(next) {
                    stack.extend(edges.get(next).into_iter().flatten().copied());
                }
            }
            false
        };
        let mut boxed = BTreeSet::new();
        for name in &self.user_types {
            for ctor in self.ctors_of(name) {
                for (index, (_, field)) in ctor.fields.iter().enumerate() {
                    let mut mentioned = BTreeSet::new();
                    self.inline_types(field, &mut mentioned);
                    if mentioned.iter().any(|other| reaches(other, name)) {
                        boxed.insert((ctor.name.to_string(), index));
                    }
                }
            }
        }
        boxed
    }

    // ---- derives -------------------------------------------------------------------------

    /// User types (and `Ui`) for which `contains` holds of some field, closed under nesting:
    /// a type is in the set when a field mentions a matching type anywhere, including the
    /// type arguments of other types. A type's own parameters do not count.
    fn closure(&self, base: &[&str], functions: bool) -> BTreeSet<String> {
        let mut set: BTreeSet<String> = base.iter().map(|name| name.to_string()).collect();
        loop {
            let mut changed = false;
            for name in &self.user_types {
                if set.contains(*name) {
                    continue;
                }
                let found = self.ctors_of(name).iter().any(|ctor| {
                    ctor.fields
                        .iter()
                        .any(|(_, field)| mentions(field, &set, functions))
                });
                if found {
                    set.insert(name.to_string());
                    changed = true;
                }
            }
            if !changed {
                return set;
            }
        }
    }

    fn derives(&self, name: &str) -> (Vec<&'static str>, bool) {
        let with_functions = self.closure(&[], true);
        if with_functions.contains(name) {
            return (vec!["Clone"], false);
        }
        let mut derives = vec!["Clone", "Debug"];
        // The checker's equality rule: no function and no `Ui` inside.
        if !self.closure(&["Ui"], true).contains(name) {
            derives.push("PartialEq");
        }
        // `Duration` (and `Ui`) have no wire form.
        let serde = !self.closure(&["Ui", "Duration"], true).contains(name);
        if serde {
            derives.extend(["::serde::Serialize", "::serde::Deserialize"]);
        }
        (derives, serde)
    }

    pub fn declarations(&self) -> String {
        let mut output = String::new();
        let none = BTreeMap::new();
        for ty in &self.program.types {
            let (name, params) = match ty {
                CoreType::Sum { name, .. } if name == "Result" => continue,
                CoreType::Record { name, params, .. } | CoreType::Sum { name, params, .. } => {
                    (name, params.as_slice())
                }
                CoreType::Distinct { name, .. } => (name, &[][..]),
            };
            let (derives, serde) = self.derives(name);
            output.push_str(&format!("#[derive({})]\n", derives.join(", ")));
            let generics = if params.is_empty() {
                String::new()
            } else {
                format!("<{}>", params.join(", "))
            };
            match ty {
                CoreType::Record { fields, .. } => {
                    output.push_str(&format!("pub struct {name}{generics} {{\n"));
                    for (index, (field, field_ty)) in fields.iter().enumerate() {
                        let ty = self.stored_type(name, index, field_ty, &none);
                        output.push_str(&field_line("pub ", field, &ty, serde));
                    }
                    if let Some(phantom) = self.phantom(name) {
                        let skip = if serde { "#[serde(skip)]\n" } else { "" };
                        output.push_str(&format!("{skip}pub {PHANTOM_FIELD}: {phantom},\n"));
                    }
                    output.push_str("}\n\n");
                }
                CoreType::Sum { variants, .. } => {
                    output.push_str(&format!("pub enum {name}{generics} {{\n"));
                    for (variant, fields) in variants {
                        if serde {
                            // The `bind-json` name of a variant (as in the Elm codecs).
                            output.push_str(&format!(
                                "#[serde(rename = \"{}\")]\n",
                                crate::certified::lower_camel(variant)
                            ));
                        }
                        if fields.is_empty() {
                            output.push_str(&format!("{variant},\n"));
                            continue;
                        }
                        output.push_str(&format!("{variant} {{\n"));
                        for (index, (field, field_ty)) in fields.iter().enumerate() {
                            let ty = self.stored_type(variant, index, field_ty, &none);
                            output.push_str(&field_line("", field, &ty, serde));
                        }
                        output.push_str("},\n");
                    }
                    if let Some(phantom) = self.phantom(name) {
                        // Uninhabited: only gives the unused parameters a use.
                        let skip = if serde { "#[serde(skip)]\n" } else { "" };
                        output.push_str(&format!(
                            "{skip}{PHANTOM_VARIANT}({phantom}, ::std::convert::Infallible),\n"
                        ));
                    }
                    output.push_str("}\n\n");
                }
                CoreType::Distinct { underlying, .. } => {
                    if serde {
                        output.push_str("#[serde(transparent)]\n");
                    }
                    let ty = self.stored_type(name, 0, underlying, &none);
                    output.push_str(&format!("pub struct {name}(pub {ty});\n\n"));
                }
            }
        }
        output
    }

    /// `PhantomData<fn() -> (P, …)>` over the type parameters of `type_name` that no field
    /// mentions (Rust rejects unused parameters, E0392); `None` when every one is used.
    pub fn phantom(&self, type_name: &str) -> Option<String> {
        let params = self.program.types.iter().find_map(|ty| match ty {
            CoreType::Record { name, params, .. } | CoreType::Sum { name, params, .. }
                if name == type_name && name != "Result" =>
            {
                Some(params)
            }
            _ => None,
        })?;
        let mut used = BTreeSet::new();
        for ctor in self.ctors_of(type_name) {
            for (_, field) in &ctor.fields {
                markers(field, &mut used);
            }
        }
        let unused: Vec<&String> = params
            .iter()
            .filter(|param| !used.contains(&format!("'{param}")))
            .collect();
        if unused.is_empty() {
            return None;
        }
        let list: String = unused.iter().map(|param| format!("{param}, ")).collect();
        Some(format!(
            "::std::marker::PhantomData<fn() -> ({})>",
            list.trim_end_matches(' ')
        ))
    }

    /// The declared type of field `index` of `ctor`, boxed when it is recursive.
    fn stored_type(
        &self,
        ctor: &str,
        index: usize,
        ty: &Type,
        generics: &BTreeMap<usize, String>,
    ) -> String {
        let rendered = self.ty(ty, generics);
        if self.is_boxed(ctor, index) {
            format!("{BOX}<{rendered}>")
        } else {
            rendered
        }
    }
}

/// The hidden field of a record whose type parameters are not all used by its fields.
pub const PHANTOM_FIELD: &str = "__phantom";
/// The hidden, uninhabited variant of such a sum type.
pub const PHANTOM_VARIANT: &str = "__Phantom";

fn markers(ty: &Type, output: &mut BTreeSet<String>) {
    match ty {
        Type::Con(name, args) if args.is_empty() && name.starts_with('\'') => {
            output.insert(name.clone());
        }
        Type::Con(_, args) => args.iter().for_each(|arg| markers(arg, output)),
        Type::Fun(params, result, _) => {
            params.iter().for_each(|param| markers(param, output));
            markers(result, output);
        }
        Type::Var(_) | Type::Error => {}
    }
}

fn field_line(visibility: &str, field: &str, ty: &str, serde: bool) -> String {
    let ident = field_ident(field);
    let rename = if serde && ident.ends_with('_') && !field.ends_with('_') {
        format!("#[serde(rename = \"{field}\")]\n")
    } else {
        String::new()
    };
    format!("{rename}{visibility}{ident}: {ty},\n")
}

fn mentions(ty: &Type, set: &BTreeSet<String>, functions: bool) -> bool {
    match ty {
        Type::Fun(..) => functions,
        Type::Con(name, args) => {
            set.contains(name) || args.iter().any(|arg| mentions(arg, set, functions))
        }
        Type::Var(_) | Type::Error => false,
    }
}

/// Rust keywords that can be written as raw identifiers (`r#type`).
const RAW_KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "else", "enum", "extern", "false", "fn", "for", "if",
    "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "static",
    "struct", "trait", "true", "type", "unsafe", "use", "where", "while", "async", "await", "dyn",
    "abstract", "become", "box", "do", "final", "macro", "override", "priv", "typeof", "unsized",
    "virtual", "yield", "try", "gen",
];

/// The Rust identifier of a record or variant field: the Bind name verbatim, as a raw
/// identifier when it is a keyword (serde strips `r#`), or with a trailing `_` (and a serde
/// rename) for the keywords that cannot be raw.
pub fn field_ident(name: &str) -> String {
    if RAW_KEYWORDS.contains(&name) {
        format!("r#{name}")
    } else if matches!(name, "self" | "Self" | "super" | "crate") {
        format!("{name}_")
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::lower::tests::compile;

    #[test]
    fn scalar_and_generic_types() {
        let program = compile("data class Caixa<T>(val conteudo: T)\n");
        let caixa = Type::Con("Caixa".into(), vec![Type::list(Type::con("Int"))]);
        assert_eq!(
            rust_type(&caixa, &program),
            "Caixa<::std::sync::Arc<::std::vec::Vec<crate::bind_runtime::Int>>>"
        );
        assert_eq!(
            rust_type(&Type::option(Type::con("Money")), &program),
            "::std::option::Option<::rust_decimal::Decimal>"
        );
        assert_eq!(rust_type(&Type::con("Boolean"), &program), "bool");
    }

    #[test]
    fn shadowed_primitive_is_qualified() {
        let program = compile("data class bool(val x: Int)\n");
        assert_eq!(
            rust_type(&Type::con("Boolean"), &program),
            "::std::primitive::bool"
        );
    }

    #[test]
    fn recursive_fields_are_boxed_only_in_cycles() {
        let program = compile(
            "sealed class Arvore {\n    data class No(val esq: Arvore, val valor: Int, val filhos: List<Arvore>, val dir: Arvore?)\n    data object Folha\n}\ndata class Par(val a: Int, val b: Arvore)\n",
        );
        let layout = Layout::new(&program);
        assert!(layout.is_boxed("No", 0));
        assert!(!layout.is_boxed("No", 1));
        assert!(!layout.is_boxed("No", 2));
        assert!(layout.is_boxed("No", 3));
        assert!(!layout.is_boxed("Par", 1));
    }

    #[test]
    fn mutual_recursion_is_boxed() {
        let program = compile(
            "data class A(val b: B?)\ndata class B(val a: A, val n: Int)\ndata class C(val a: A)\n",
        );
        let layout = Layout::new(&program);
        assert!(layout.is_boxed("A", 0));
        assert!(layout.is_boxed("B", 0));
        assert!(!layout.is_boxed("C", 0));
    }

    #[test]
    fn derives_follow_contents() {
        let program = compile(
            "data class F(val f: (Int) -> Int)\ndata class W(val f: F)\ndata class D(val d: Duration)\ndata class P(val x: Int)\n",
        );
        let output = type_declarations(&program);
        assert!(output.contains("#[derive(Clone)]\npub struct F "));
        assert!(output.contains("#[derive(Clone)]\npub struct W "));
        assert!(output.contains("#[derive(Clone, Debug, PartialEq)]\npub struct D "));
        assert!(output.contains(
            "#[derive(Clone, Debug, PartialEq, ::serde::Serialize, ::serde::Deserialize)]\npub struct P "
        ));
    }

    #[test]
    fn keyword_fields() {
        assert_eq!(field_ident("type"), "r#type");
        assert_eq!(field_ident("self"), "self_");
        assert_eq!(field_ident("nome"), "nome");
    }
}
