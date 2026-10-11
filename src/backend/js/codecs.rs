//! JSON codecs of the JS frontend, built only from IR types: decoders (server JSON → values)
//! on the runtime's decoder combinators, and encoders (values → JSON-ready data for `rpc`).
//!
//! The JSON is exactly the generated server's (serde, see `backend::rust::types`): records are
//! objects with the original field names; a field-less variant is the string
//! `certified::lower_camel(variant)`, a variant with fields is `{lowerCamel: {fields}}`;
//! `Result` is `{"Ok": v}` / `{"Err": e}`; a distinct value is its underlying value; `Int`,
//! `Decimal` and `Money` are strings; `Long` is a number; `Unit` and `None` are `null`;
//! `Option<Option<T>>` is just as serde writes it (`Some(None)` is `null`); `List`/`Set` are
//! arrays and `Map` an array of `[key, value]` pairs.
//!
//! In memory, record and variant fields live under their escaped names (`names::js_value`),
//! so a field named `tag`, `__proto__` or `constructor` is an ordinary property of a
//! null-prototype object and never meets the variant tag or `Object.prototype`.
//!
//! User types get one named helper per instance (`dec<n>` / `enc<n>`), so recursive types
//! decode and encode through hoisted functions.

use std::collections::BTreeMap;

use crate::core::ir::{CoreProgram, CoreType};
use crate::types::ty::{Type, display};

use super::names::{js_string, js_value};

/// A JS expression evaluating to a decoder of `ty` (self-contained: an IIFE when helpers are
/// needed).
pub fn decoder(ty: &Type, program: &CoreProgram) -> String {
    let mut codecs = Codecs::new(program);
    let decoder = codecs.decoder(ty);
    codecs.standalone(decoder)
}

/// A JS expression evaluating to an encoder `(v) => json` of `ty` (self-contained).
pub fn encoder(ty: &Type, program: &CoreProgram) -> String {
    let mut codecs = Codecs::new(program);
    let body = codecs.encode(ty, "v");
    codecs.standalone(format!("(v) => {body}"))
}

/// The codec helpers of one module.
pub struct Codecs<'p> {
    program: &'p CoreProgram,
    /// Helper name of each `(kind, instance)` already defined.
    helpers: BTreeMap<(Kind, String), String>,
    definitions: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Decoder,
    Encoder,
}

/// What a user type name refers to, with its fields at a concrete instance.
enum UserType {
    Record(Vec<(String, Type)>),
    Sum(Vec<(String, Vec<(String, Type)>)>),
    Distinct(Type),
}

impl<'p> Codecs<'p> {
    pub fn new(program: &'p CoreProgram) -> Codecs<'p> {
        Codecs {
            program,
            helpers: BTreeMap::new(),
            definitions: Vec::new(),
        }
    }

    /// The helper definitions made so far, in order.
    pub fn definitions(&self) -> String {
        self.definitions.join("\n")
    }

    fn standalone(&self, expression: String) -> String {
        if self.definitions.is_empty() {
            expression
        } else {
            format!(
                "(() => {{\n{}\nreturn {expression};\n}})()",
                self.definitions()
            )
        }
    }

    /// A JS expression evaluating to a decoder of `ty`.
    pub fn decoder(&mut self, ty: &Type) -> String {
        let Type::Con(name, args) = ty else {
            return no_wire_form(ty);
        };
        if self.user_type(name, args).is_some() && name != "Result" {
            return self.helper(Kind::Decoder, ty);
        }
        match (name.as_str(), args.as_slice()) {
            ("Int", []) => "rt.decodeInt".into(),
            ("Long", []) => "rt.decodeLong".into(),
            ("Double", []) => "rt.decodeDouble".into(),
            ("Float", []) => "rt.decodeFloat".into(),
            ("Boolean", []) => "rt.decodeBool".into(),
            ("Unit", []) => "rt.decodeUnit".into(),
            ("Decimal" | "Money", []) => "rt.decodeDecimal".into(),
            (name, []) if is_text(name) => "rt.decodeString".into(),
            // JSON null is `None`; any other value is `Some` (`{some: v}` in memory).
            ("Option", [inner]) => format!("rt.decodeOptional({})", self.decoder(inner)),
            ("List" | "Set", [item]) => format!("rt.decodeList({})", self.decoder(item)),
            ("Map", [key, value]) => {
                let key = self.decoder(key);
                let value = self.decoder(value);
                format!("rt.decodeList(rt.decodePair({key}, {value}))")
            }
            ("Result", [ok, err]) => {
                let ok = self.decoder(ok);
                let err = self.decoder(err);
                format!("rt.decodeResult({ok}, {err})")
            }
            _ => no_wire_form(ty),
        }
    }

    /// A JS expression giving the JSON-ready form of `value` (a variable or property path,
    /// which may be read more than once) of type `ty`.
    pub fn encode(&mut self, ty: &Type, value: &str) -> String {
        self.encode_at(ty, value, 0)
    }

    fn encode_at(&mut self, ty: &Type, value: &str, depth: usize) -> String {
        let Type::Con(name, args) = ty else {
            return no_encoding(ty);
        };
        if self.user_type(name, args).is_some() && name != "Result" {
            let helper = self.helper(Kind::Encoder, ty);
            return format!("{helper}({value})");
        }
        match (name.as_str(), args.as_slice()) {
            ("Int" | "Decimal" | "Money", []) => format!("{value}.toString()"),
            ("Long", []) => format!("rt.encodeLong({value})"),
            ("Unit", []) => "null".into(),
            ("Double" | "Float" | "Boolean", []) => value.to_string(),
            (name, []) if is_text(name) => value.to_string(),
            ("Option", [inner]) => {
                let inner = self.encode_at(inner, &format!("{value}.some"), depth);
                format!("({value} === null ? null : {inner})")
            }
            ("List" | "Set", [item]) => {
                let x = format!("x{depth}");
                let item = self.encode_at(item, &x, depth + 1);
                format!("{value}.map(({x}) => {item})")
            }
            ("Map", [key, item]) => {
                let x = format!("x{depth}");
                let key = self.encode_at(key, &format!("{x}[0]"), depth + 1);
                let item = self.encode_at(item, &format!("{x}[1]"), depth + 1);
                format!("{value}.map(({x}) => [{key}, {item}])")
            }
            ("Result", [ok, err]) => {
                let ok = self.encode_at(ok, &format!("{value}.value"), depth);
                let err = self.encode_at(err, &format!("{value}.value"), depth);
                format!(
                    "({value}.tag === \"Ok\" ? rt.record([[\"Ok\", {ok}]]) : rt.record([[\"Err\", {err}]]))"
                )
            }
            _ => no_encoding(ty),
        }
    }

    /// The named helper of a user type instance, defined on first use.
    fn helper(&mut self, kind: Kind, ty: &Type) -> String {
        let key = (kind, format!("{ty:?}"));
        if let Some(name) = self.helpers.get(&key) {
            return name.clone();
        }
        let prefix = match kind {
            Kind::Decoder => "dec",
            Kind::Encoder => "enc",
        };
        let name = format!("{prefix}{}", self.helpers.len());
        self.helpers.insert(key, name.clone());
        let Type::Con(type_name, args) = ty else {
            return name;
        };
        let Some(user) = self.user_type(type_name, args) else {
            return name;
        };
        let definition = match kind {
            Kind::Decoder => self.decoder_helper(&name, ty, user),
            Kind::Encoder => self.encoder_helper(&name, ty, user),
        };
        self.definitions.push(definition);
        name
    }

    fn decoder_helper(&mut self, name: &str, ty: &Type, user: UserType) -> String {
        let fields = |codecs: &mut Codecs, fields: &[(String, Type)]| -> String {
            let entries: Vec<String> = fields
                .iter()
                .map(|(field, field_ty)| {
                    format!(
                        "[{}, {}, {}]",
                        js_string(field),
                        codecs.decoder(field_ty),
                        js_string(&js_value(field))
                    )
                })
                .collect();
            format!("[{}]", entries.join(", "))
        };
        let mut optional = false;
        let decoder = match &user {
            UserType::Record(record) => format!("rt.decodeRecord({})", fields(self, record)),
            UserType::Sum(variants) => {
                let cases: Vec<String> = variants
                    .iter()
                    .map(|(variant, variant_fields)| {
                        let wire = js_string(&crate::certified::lower_camel(variant));
                        let tag = js_string(variant);
                        if variant_fields.is_empty() {
                            format!("[{wire}, {tag}, null]")
                        } else {
                            format!("[{wire}, {tag}, {}]", fields(self, variant_fields))
                        }
                    })
                    .collect();
                format!("rt.decodeVariant([\n{}\n])", cases.join(",\n"))
            }
            UserType::Distinct(underlying) => {
                // serde reads a missing field as `None` through a transparent wrapper too.
                optional = self.is_optional_wire(underlying);
                self.decoder(underlying)
            }
        };
        let mark = if optional {
            format!("\n{name}.optional = true;")
        } else {
            String::new()
        };
        format!(
            "// Decoder of {}.\nlet {name}_ = null;\nfunction {name}(json) {{\nreturn ({name}_ ??= {decoder})(json);\n}}{mark}\n",
            display(ty)
        )
    }

    fn encoder_helper(&mut self, name: &str, ty: &Type, user: UserType) -> String {
        let record = |codecs: &mut Codecs, fields: &[(String, Type)]| -> String {
            let entries: Vec<String> = fields
                .iter()
                .map(|(field, field_ty)| {
                    let value = format!("v.{}", js_value(field));
                    format!(
                        "[{}, {}]",
                        js_string(field),
                        codecs.encode(field_ty, &value)
                    )
                })
                .collect();
            format!("rt.record([{}])", entries.join(", "))
        };
        let body = match &user {
            UserType::Record(fields) => format!("return {};", record(self, fields)),
            UserType::Sum(variants) => {
                let mut lines = Vec::new();
                for (variant, fields) in variants {
                    let wire = js_string(&crate::certified::lower_camel(variant));
                    let value = if fields.is_empty() {
                        wire
                    } else {
                        format!("rt.record([[{wire}, {}]])", record(self, fields))
                    };
                    lines.push(format!(
                        "if (v.tag === {}) return {value};",
                        js_string(variant)
                    ));
                }
                lines.push(format!(
                    "throw new TypeError({});",
                    js_string(&format!("bindc: not a value of {}", display(ty)))
                ));
                lines.join("\n")
            }
            UserType::Distinct(underlying) => format!("return {};", self.encode(underlying, "v")),
        };
        format!(
            "// Encoder of {}.\nfunction {name}(v) {{\n{body}\n}}\n",
            display(ty)
        )
    }

    /// Whether `ty`'s wire form is serde's `Option` (directly or through distinct types).
    fn is_optional_wire(&self, ty: &Type) -> bool {
        let mut current = ty.clone();
        for _ in 0..16 {
            let Type::Con(name, args) = &current else {
                return false;
            };
            match self.user_type(name, args) {
                Some(UserType::Distinct(underlying)) => current = underlying,
                Some(_) => return false,
                None => return name == "Option",
            }
        }
        false
    }

    /// The declaration of the user type `name` at the instance `args`. `Result` is a
    /// program type too (lowered first), but has its own wire form.
    fn user_type(&self, name: &str, args: &[Type]) -> Option<UserType> {
        let declaration = self.program.types.iter().find(|ty| match ty {
            CoreType::Record { name: n, .. }
            | CoreType::Sum { name: n, .. }
            | CoreType::Distinct { name: n, .. } => n == name,
        })?;
        let substitution = |params: &[String]| -> BTreeMap<String, Type> {
            params
                .iter()
                .zip(args)
                .map(|(param, arg)| (format!("'{param}"), arg.clone()))
                .collect()
        };
        let fields = |fields: &[(String, Type)], markers: &BTreeMap<String, Type>| {
            fields
                .iter()
                .map(|(field, ty)| (field.clone(), substitute(ty, markers)))
                .collect::<Vec<_>>()
        };
        Some(match declaration {
            CoreType::Record {
                params,
                fields: record,
                ..
            } => UserType::Record(fields(record, &substitution(params))),
            CoreType::Sum {
                params, variants, ..
            } => {
                let markers = substitution(params);
                UserType::Sum(
                    variants
                        .iter()
                        .map(|(variant, variant_fields)| {
                            (variant.clone(), fields(variant_fields, &markers))
                        })
                        .collect(),
                )
            }
            CoreType::Distinct { underlying, .. } => UserType::Distinct(underlying.clone()),
        })
    }
}

/// Replaces the `'P` markers of declared field types by the instance's types.
pub(super) fn substitute(ty: &Type, markers: &BTreeMap<String, Type>) -> Type {
    match ty {
        Type::Con(name, args) if args.is_empty() && name.starts_with('\'') => {
            markers.get(name).cloned().unwrap_or_else(|| ty.clone())
        }
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

/// Text types: a canonical JSON string (the server validates their format).
fn is_text(name: &str) -> bool {
    matches!(
        name,
        "String"
            | "Email"
            | "Url"
            | "DatabaseUrl"
            | "Secret"
            | "UUID"
            | "Date"
            | "DateTime"
            | "Instant"
    )
}

/// A decoder that always fails: the type has no wire form (functions, `Ui`, `Duration`, type
/// variables). The checker keeps such types out of endpoints; this only keeps the module valid.
fn no_wire_form(ty: &Type) -> String {
    let message = js_string(&format!("bindc: {} has no JSON form", display(ty)));
    format!("((json) => {{ throw new rt.DecodeError({message}); }})")
}

fn no_encoding(ty: &Type) -> String {
    let message = js_string(&format!("bindc: {} has no JSON form", display(ty)));
    format!("(() => {{ throw new TypeError({message}); }})()")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(source: &str) -> CoreProgram {
        crate::core::lower::tests::compile(source)
    }

    #[test]
    fn scalars_and_containers() {
        let program = program("fun f() = 1\n");
        let ty = |name: &str| Type::con(name);
        assert_eq!(decoder(&ty("Int"), &program), "rt.decodeInt");
        assert_eq!(decoder(&ty("Money"), &program), "rt.decodeDecimal");
        assert_eq!(decoder(&ty("UUID"), &program), "rt.decodeString");
        assert_eq!(
            decoder(&Type::list(Type::option(ty("Int"))), &program),
            "rt.decodeList(rt.decodeOptional(rt.decodeInt))"
        );
        assert_eq!(
            decoder(&Type::option(Type::option(ty("Int"))), &program),
            "rt.decodeOptional(rt.decodeOptional(rt.decodeInt))"
        );
        assert_eq!(
            encoder(&Type::list(Type::option(ty("Int"))), &program),
            "(v) => v.map((x0) => (x0 === null ? null : x0.some.toString()))"
        );
        assert_eq!(
            encoder(&Type::option(Type::option(ty("Decimal"))), &program),
            "(v) => (v === null ? null : (v.some === null ? null : v.some.some.toString()))"
        );
    }

    #[test]
    fn user_types_use_original_wire_names_and_escaped_properties() {
        let program = program(
            "sealed class Forma {\n    data class CirculoGrande(val tag: Int)\n    data object NADA\n}\ndata class P(val __proto__: String, val f: Forma)\ndistinct type Talvez = Int?\nfun f() = 1\n",
        );
        let decode = decoder(&Type::con("P"), &program);
        assert!(
            decode.contains(
                "rt.decodeRecord([[\"__proto__\", rt.decodeString, \"u_____proto____\"], [\"f\", dec1, \"u_f\"]])"
            ),
            "{decode}"
        );
        assert!(
            decode.contains(
                "[\"circuloGrande\", \"CirculoGrande\", [[\"tag\", rt.decodeInt, \"u_tag\"]]]"
            ),
            "{decode}"
        );
        assert!(
            decode.contains(&format!(
                "[{}, \"NADA\", null]",
                js_string(&crate::certified::lower_camel("NADA"))
            )),
            "{decode}"
        );
        let encode = encoder(&Type::con("P"), &program);
        assert!(
            encode
                .contains("rt.record([[\"__proto__\", v.u_____proto____], [\"f\", enc1(v.u_f)]])"),
            "{encode}"
        );
        assert!(encode.contains("if (v.tag === \"CirculoGrande\") return rt.record([[\"circuloGrande\", rt.record([[\"tag\", v.u_tag.toString()]])]]);"), "{encode}");
        let maybe = decoder(&Type::con("Talvez"), &program);
        assert!(maybe.contains("dec0.optional = true;"), "{maybe}");
    }
}
