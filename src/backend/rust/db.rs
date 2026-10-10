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

use crate::core::ir::{CoreFunction, CoreProgram, CoreType, DbOp, Expr, ExprKind};
use crate::schema::DatabaseSchema;
use crate::types::ty::Type;

use super::types::{Layout, RUNTIME, STRING};

/// One database operation used by the generated code.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DbKey {
    Require(String),
    Find(String),
    /// Table and the assigned columns, in source order.
    Insert(String, Vec<String>),
}

impl DbKey {
    pub fn of(op: &DbOp, table: &str) -> DbKey {
        match op {
            DbOp::Require => DbKey::Require(table.to_string()),
            DbOp::Find => DbKey::Find(table.to_string()),
            DbOp::Insert { columns } => DbKey::Insert(table.to_string(), columns.clone()),
        }
    }

    fn table(&self) -> &str {
        match self {
            DbKey::Require(table) | DbKey::Find(table) | DbKey::Insert(table, _) => table,
        }
    }
}

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

/// The database operations in `functions` (lambdas included).
pub fn operations(functions: &[&CoreFunction]) -> BTreeSet<DbKey> {
    fn walk(expr: &Expr, output: &mut BTreeSet<DbKey>) {
        if let ExprKind::Db { op, table, .. } = &expr.kind {
            output.insert(DbKey::of(op, table));
        }
        super::children(expr)
            .into_iter()
            .for_each(|child| walk(child, output));
    }
    let mut output = BTreeSet::new();
    for function in functions {
        walk(&function.body, &mut output);
    }
    output
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

fn field_type<'p>(model: &Model<'p>, column: &str) -> Result<&'p Type, String> {
    model
        .fields
        .iter()
        .find(|(name, _)| name == column)
        .map(|(_, ty)| ty)
        .ok_or_else(|| format!("Model `{}` has no field `{column}`", model.name))
}

impl DbPlan {
    pub fn new(
        keys: &BTreeSet<DbKey>,
        program: &CoreProgram,
        database: &DatabaseSchema,
    ) -> Result<DbPlan, String> {
        // Inserts into one table with different column lists get numbered names.
        let mut insert_lists: BTreeMap<&str, usize> = BTreeMap::new();
        for key in keys {
            if let DbKey::Insert(table, _) = key {
                *insert_lists.entry(table).or_default() += 1;
            }
        }
        let mut numbers: BTreeMap<&str, usize> = BTreeMap::new();
        let mut functions = BTreeMap::new();
        let mut tables = BTreeSet::new();
        for key in keys {
            let table = key.table();
            let model = model(program, database, table)?;
            let definition = &database.tables[table];
            let (name, params) = match key {
                DbKey::Require(_) | DbKey::Find(_) => {
                    let verb = if matches!(key, DbKey::Require(_)) {
                        "require"
                    } else {
                        "find"
                    };
                    let key_type = field_type(&model, &definition.primary_key)?.clone();
                    (format!("db_{verb}_{}", identifier(table)), vec![key_type])
                }
                DbKey::Insert(_, columns) => {
                    let params = columns
                        .iter()
                        .map(|column| field_type(&model, column).cloned())
                        .collect::<Result<Vec<_>, _>>()?;
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
        Ok(DbPlan { functions, tables })
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
            let result = match key {
                DbKey::Find(_) => format!("::std::option::Option<{}>", model.name),
                _ => model.name.to_string(),
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
            output.push_str(&format!(
                "fn decode_{}(row: &::sqlx::postgres::PgRow) -> ::std::result::Result<{}, {RUNTIME}::AppError> {{\n::std::result::Result::Ok({} {{\n{}\n}})\n}}\n\n",
                identifier(table),
                model.name,
                model.name,
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
    let unwrap = ".0".repeat(column.wrappers.len());
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
                format!("{wrapper}({value})")
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

/// A Rust identifier fragment for a table or column name.
fn identifier(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}
