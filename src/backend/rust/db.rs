//! Database access of the generated server: one function per `(table, operation)` used by the
//! endpoints, running parametrized SQL on the request's transaction (`Ctx::transaction`).
//!
//! Every column is cast in SQL to the PostgreSQL type its Bind type decodes from exactly, both
//! as a parameter (`$1::numeric`) and when selected (`"c"::numeric AS "c"`), so a column of a
//! compatible type (an `integer` column for an `Int`, a `varchar` for a `String`, …) still
//! decodes into the Bind representation. An `Int` outside an `integer` column's range is
//! rejected by PostgreSQL on insert: a database error, answered with HTTP 500.
//!
//! The transaction guard is taken only after every argument has been evaluated (they are the
//! function's parameters) and dropped as soon as the query returns, so no Bind code runs while
//! it is held.

use std::collections::{BTreeMap, BTreeSet};

pub use crate::core::analysis::{DbKey, operations};
use crate::core::ir::{CoreProgram, CoreType};
use crate::schema::DatabaseSchema;
use crate::types::ty::Type;

use super::types::{DISTINCT_FIELD, Layout, RUNTIME, STRING};

/// What a call site needs: the function's name and its parameter types.
#[derive(Clone, Debug)]
pub struct DbFunction {
    pub name: String,
    pub params: Vec<Type>,
}

/// The database functions for `keys`, with their names and parameter types.
pub struct DbPlan {
    pub functions: BTreeMap<DbKey, DbFunction>,
    /// Tables whose rows are decoded.
    tables: BTreeSet<String>,
}

/// A table's model: its name and fields (`entity` declarations are records).
struct Model<'p> {
    name: &'p str,
    fields: &'p [(String, Type)],
}

fn model<'p>(
    program: &'p CoreProgram,
    database: &DatabaseSchema,
    table: &str,
) -> Result<Model<'p>, String> {
    let definition = database
        .tables
        .get(table)
        .ok_or_else(|| format!("Unknown database table `{table}`"))?;
    program
        .types
        .iter()
        .find_map(|ty| match ty {
            CoreType::Record {
                name,
                params,
                fields,
            } if *name == definition.model && params.is_empty() => Some(Model { name, fields }),
            _ => None,
        })
        .ok_or_else(|| {
            format!(
                "Table `{table}` maps to `{}`, which is not a record without type parameters",
                definition.model
            )
        })
}

/// Why the database layer of a verified program cannot be generated. `verify` reports each
/// one (E8310) by building the same [`DbPlan`] the backend uses.
#[derive(Clone, Debug, PartialEq)]
pub struct DbError {
    pub table: String,
    /// The table's model, when it is a record.
    pub model: Option<String>,
    /// The column concerned, if any.
    pub column: Option<String>,
    pub message: String,
}

impl DbPlan {
    /// The functions for `keys`, or every reason they cannot be generated: a table without a
    /// record model, a primary key or inserted column the model does not declare, a model
    /// field whose type has no column mapping.
    pub fn new(
        keys: &BTreeSet<DbKey>,
        program: &CoreProgram,
        database: &DatabaseSchema,
    ) -> Result<DbPlan, Vec<DbError>> {
        let profile = crate::certified::profile_version();
        let layout = Layout::new(program);
        let mut errors = Vec::new();
        // Inserts into one table with different column lists get numbered names.
        let mut insert_lists: BTreeMap<&str, usize> = BTreeMap::new();
        for key in keys {
            if let DbKey::Insert(table, _) = key {
                *insert_lists.entry(table).or_default() += 1;
            }
        }
        let mut checked_tables = BTreeSet::new();
        let mut numbers: BTreeMap<&str, usize> = BTreeMap::new();
        let mut functions = BTreeMap::new();
        let mut tables = BTreeSet::new();
        for key in keys {
            let table = key.table();
            let error = |model: Option<&str>, column: Option<&str>, message: String| DbError {
                table: table.to_string(),
                model: model.map(str::to_string),
                column: column.map(str::to_string),
                message,
            };
            let model = match model(program, database, table) {
                Ok(model) => model,
                Err(message) => {
                    if checked_tables.insert(table) {
                        errors.push(error(None, None, format!("{message}, so the generated server cannot access it in profile {profile}.")));
                    }
                    continue;
                }
            };
            if checked_tables.insert(table) {
                // Every field is read back (selected and decoded) by every operation.
                for (field, ty) in model.fields {
                    if column(ty, &layout).is_err() {
                        errors.push(error(
                            Some(model.name),
                            Some(field),
                            format!(
                                "Column `{table}.{field}` has type `{}`, which has no database mapping in profile {profile}.",
                                crate::types::ty::display(ty)
                            ),
                        ));
                    }
                }
            }
            let definition = &database.tables[table];
            let mut field = |column: &str, role: &str| -> Option<Type> {
                let found = model
                    .fields
                    .iter()
                    .find(|(name, _)| name == column)
                    .map(|(_, ty)| ty.clone());
                if found.is_none() {
                    errors.push(error(
                        Some(model.name),
                        Some(column),
                        format!(
                            "Column `{table}.{column}` is {role}, but model `{}` declares no field `{column}`, so the generated server cannot use it in profile {profile}.",
                            model.name
                        ),
                    ));
                }
                found
            };
            let (name, params) = match key {
                DbKey::Require(_) | DbKey::Find(_) => {
                    let verb = if matches!(key, DbKey::Require(_)) {
                        "require"
                    } else {
                        "find"
                    };
                    let Some(key_type) =
                        field(&definition.primary_key, "the primary key of the table")
                    else {
                        continue;
                    };
                    (format!("db_{verb}_{}", identifier(table)), vec![key_type])
                }
                DbKey::Insert(_, columns) => {
                    let params: Vec<Option<Type>> = columns
                        .iter()
                        .map(|column| field(column, "assigned by an insert"))
                        .collect();
                    let Some(params) = params.into_iter().collect::<Option<Vec<_>>>() else {
                        continue;
                    };
                    let name = if insert_lists[table] > 1 {
                        let number = numbers.entry(table).or_default();
                        *number += 1;
                        format!("db_insert_{}_{number}", identifier(table))
                    } else {
                        format!("db_insert_{}", identifier(table))
                    };
                    (name, params)
                }
            };
            tables.insert(table.to_string());
            functions.insert(key.clone(), DbFunction { name, params });
        }
        if errors.is_empty() {
            Ok(DbPlan { functions, tables })
        } else {
            Err(errors)
        }
    }

    /// The database functions and the row decoders they use.
    pub fn render(&self, layout: &Layout, database: &DatabaseSchema) -> Result<String, String> {
        let program = layout.program;
        let mut output = String::new();
        for (key, function) in &self.functions {
            let table = key.table();
            let model = model(program, database, table)?;
            let definition = &database.tables[table];
            let projection = model
                .fields
                .iter()
                .map(|(field, ty)| {
                    Ok(format!(
                        "{}{} AS {}",
                        quote(field),
                        cast(ty, layout)?,
                        quote(field)
                    ))
                })
                .collect::<Result<Vec<_>, String>>()?
                .join(", ");
            let (sql, columns, fetch) = match key {
                DbKey::Require(_) | DbKey::Find(_) => {
                    let key_type = &function.params[0];
                    let sql = format!(
                        "SELECT {projection} FROM {} WHERE {} = $1{} LIMIT 1",
                        quote(table),
                        quote(&definition.primary_key),
                        cast(key_type, layout)?
                    );
                    (sql, vec![definition.primary_key.clone()], "fetch_optional")
                }
                DbKey::Insert(_, columns) => {
                    let placeholders = function
                        .params
                        .iter()
                        .enumerate()
                        .map(|(index, ty)| Ok(format!("${}{}", index + 1, cast(ty, layout)?)))
                        .collect::<Result<Vec<_>, String>>()?
                        .join(", ");
                    let names: Vec<String> = columns.iter().map(|column| quote(column)).collect();
                    let sql = format!(
                        "INSERT INTO {} ({}) VALUES ({placeholders}) RETURNING {projection}",
                        quote(table),
                        names.join(", ")
                    );
                    (sql, columns.clone(), "fetch_one")
                }
            };
            let none = BTreeMap::new();
            let mut params = vec![format!("ctx: {RUNTIME}::Ctx")];
            let mut binds = String::new();
            for (index, (column, ty)) in columns.iter().zip(&function.params).enumerate() {
                let name = format!("p{index}_{}", identifier(column));
                params.push(format!("{name}: {}", layout.ty(ty, &none)));
                binds.push_str(&format!(".bind({})", bind_value(&name, ty, layout)?));
            }
            let model_type = layout.type_path(model.name);
            let result = match key {
                DbKey::Find(_) => format!("::std::option::Option<{model_type}>"),
                _ => model_type,
            };
            let decode = format!("decode_{}", identifier(table));
            let finish = match key {
                DbKey::Require(_) => format!(
                    "match row {{\n::std::option::Option::Some(row) => {decode}(&row),\n::std::option::Option::None => ::std::result::Result::Err({RUNTIME}::AppError::NotFound({STRING}::from({:?}))),\n}}",
                    format!("{} not found", model.name)
                ),
                DbKey::Find(_) => format!("row.as_ref().map({decode}).transpose()"),
                DbKey::Insert(..) => format!("{decode}(&row)"),
            };
            output.push_str(&format!(
                "fn {}({}) -> {RUNTIME}::BoxFuture<{result}> {{\n::std::boxed::Box::pin(async move {{\nlet query = ::sqlx::query({sql:?}){binds};\nlet row = {{\nlet mut transaction = ctx.transaction().await?;\nquery.{fetch}(&mut **transaction).await?\n}};\n{finish}\n}})\n}}\n\n",
                function.name,
                params.join(", "),
            ));
        }
        for table in &self.tables {
            let model = model(program, database, table)?;
            let mut fields = Vec::new();
            for (field, ty) in model.fields {
                fields.push(format!(
                    "{}: {},",
                    super::types::field_ident(field),
                    decode_value(field, ty, layout)?
                ));
            }
            let model_type = layout.type_path(model.name);
            output.push_str(&format!(
                "fn decode_{}(row: &::sqlx::postgres::PgRow) -> ::std::result::Result<{model_type}, {RUNTIME}::AppError> {{\n::std::result::Result::Ok({model_type} {{\n{}\n}})\n}}\n\n",
                identifier(table),
                fields.join("\n")
            ));
        }
        Ok(output)
    }
}

/// A scalar column type: its decoding Rust type and PostgreSQL cast.
fn scalar(name: &str) -> Option<(&'static str, &'static str)> {
    Some(match name {
        "Int" => ("crate::bind_runtime::Int", "::numeric"),
        "Long" => ("::std::primitive::i64", "::int8"),
        "Double" => ("::std::primitive::f64", "::float8"),
        "Float" => ("::std::primitive::f32", "::float4"),
        "Boolean" => ("::std::primitive::bool", "::boolean"),
        "String" | "Email" | "Url" => ("::std::string::String", "::text"),
        "Decimal" | "Money" => ("::rust_decimal::Decimal", "::numeric"),
        "UUID" => ("::uuid::Uuid", "::uuid"),
        "Date" => ("::chrono::NaiveDate", "::date"),
        "DateTime" => ("::chrono::NaiveDateTime", "::timestamp"),
        "Instant" => ("::chrono::DateTime<::chrono::Utc>", "::timestamptz"),
        _ => return None,
    })
}

/// How a column of type `ty` is stored: optional or not, the chain of distinct wrappers
/// (outermost first) and the scalar underneath.
struct Column<'a> {
    nullable: bool,
    wrappers: Vec<&'a str>,
    scalar: &'a str,
}

fn column<'a>(ty: &'a Type, layout: &'a Layout) -> Result<Column<'a>, String> {
    let (nullable, mut current) = match ty {
        Type::Con(name, args) if name == "Option" && args.len() == 1 => (true, &args[0]),
        _ => (false, ty),
    };
    let mut wrappers = Vec::new();
    // Each step unwraps one distinct type; a chain is never longer than the program's types.
    for _ in 0..=layout.program.types.len() {
        let Type::Con(name, args) = current else {
            break;
        };
        if !args.is_empty() {
            break;
        }
        let distinct = layout
            .program
            .types
            .iter()
            .find_map(|declaration| match declaration {
                CoreType::Distinct {
                    name: distinct,
                    underlying,
                } if distinct == name => Some(underlying),
                _ => None,
            });
        if let Some(underlying) = distinct {
            wrappers.push(name.as_str());
            current = underlying;
        } else if scalar(name).is_some() {
            return Ok(Column {
                nullable,
                wrappers,
                scalar: name,
            });
        } else {
            break;
        }
    }
    Err(format!(
        "A column of type `{}` has no database mapping in the generated server",
        crate::types::ty::display(ty)
    ))
}

fn cast(ty: &Type, layout: &Layout) -> Result<&'static str, String> {
    let column = column(ty, layout)?;
    Ok(scalar(column.scalar)
        .map(|(_, cast)| cast)
        .unwrap_or_default())
}

/// The value bound for parameter `name` of type `ty`: distinct wrappers removed.
fn bind_value(name: &str, ty: &Type, layout: &Layout) -> Result<String, String> {
    let column = column(ty, layout)?;
    let unwrap = format!(".{DISTINCT_FIELD}").repeat(column.wrappers.len());
    Ok(match (column.nullable, unwrap.is_empty()) {
        (_, true) => name.to_string(),
        (false, false) => format!("{name}{unwrap}"),
        (true, false) => format!("{name}.map(|value| value{unwrap})"),
    })
}

/// Reads column `field` of type `ty` from `row`.
fn decode_value(field: &str, ty: &Type, layout: &Layout) -> Result<String, String> {
    let column = column(ty, layout)?;
    let raw = scalar(column.scalar)
        .map(|(rust, _)| rust)
        .unwrap_or_default();
    let raw = if column.nullable {
        format!("::std::option::Option<{raw}>")
    } else {
        raw.to_string()
    };
    let read = format!("::sqlx::Row::try_get::<{raw}, _>(row, {field:?})?");
    if column.wrappers.is_empty() {
        return Ok(read);
    }
    let wrap = |inner: &str| {
        column
            .wrappers
            .iter()
            .rev()
            .fold(inner.to_string(), |value, wrapper| {
                let field = if value == DISTINCT_FIELD {
                    value
                } else {
                    format!("{DISTINCT_FIELD}: {value}")
                };
                format!("{} {{ {field} }}", layout.type_path(wrapper))
            })
    };
    Ok(if column.nullable {
        format!("{read}.map(|value| {})", wrap("value"))
    } else {
        wrap(&read)
    })
}

fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// A Rust identifier fragment for a table or column name, injective: ASCII letters and
/// digits are kept, `_` becomes `__` and any other character `_x<hex>_`. A single `_` in the
/// output is always followed by `_` or `x`, so `_<digit>` (the numbering of inserts) never
/// occurs inside a fragment and names built as `db_<op>_<fragment>[_<n>]` never collide.
fn identifier(name: &str) -> String {
    let mut output = String::new();
    for ch in name.chars() {
        match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' => output.push(ch),
            '_' => output.push_str("__"),
            other => output.push_str(&format!("_x{:x}_", u32::from(other))),
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::lower::tests::{compile_with, schema};

    #[test]
    fn identifiers_are_injective() {
        let names = ["a-b", "a_b", "a__b", "a_x2d_b", "ação", "t_1", "t"];
        let fragments: BTreeSet<String> = names.iter().map(|name| identifier(name)).collect();
        assert_eq!(fragments.len(), names.len(), "{fragments:?}");
        assert_eq!(identifier("customers"), "customers");
        assert_eq!(identifier("a-b"), "a_x2d_b");
        assert_eq!(identifier("a_b"), "a__b");
    }

    #[test]
    fn numbered_inserts_do_not_collide_with_other_tables() {
        let csv = "table,model,primary_key,column,type,nullable,required,has_default,engine\nt,T,id,id,TId,false,true,true,postgresql\nt,T,id,a,String,false,true,false,postgresql\nt,T,id,b,String,true,false,false,postgresql\nt_1,U,id,id,UId,false,true,true,postgresql\nt_1,U,id,a,String,false,true,false,postgresql\n";
        let source = "distinct type TId = UUID\nentity T(val id: TId, val a: String, val b: String?)\ndistinct type UId = UUID\nentity U(val id: UId, val a: String)\nserver fun f(): TId effects [db.write] {\n    val x = db.t.insert {\n        a = \"1\"\n    }\n    val y = db.t.insert {\n        a = \"2\"\n        b = null\n    }\n    val z = db.t_1.insert {\n        a = \"4\"\n    }\n    return x.id\n}\n";
        let schema = schema(csv);
        let program = compile_with(source, &schema);
        let functions: Vec<&crate::core::ir::CoreFunction> = program.functions.iter().collect();
        let plan = DbPlan::new(&operations(&functions), &program, &schema).unwrap();
        let names: BTreeSet<&str> = plan
            .functions
            .values()
            .map(|function| function.name.as_str())
            .collect();
        assert_eq!(
            names,
            BTreeSet::from(["db_insert_t_1", "db_insert_t_2", "db_insert_t__1"])
        );
    }
}
