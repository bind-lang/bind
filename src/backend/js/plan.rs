//! The frontend plan of a web target: which component is the page, how its parameters are read
//! from the URL, and what keeps a verified program from having a frontend (E8311).
//!
//! `verify` turns [`frontend_problems`] into E8311 diagnostics and the generator
//! ([`super::page::web_files`] and the SSR server) builds from [`frontend_plan`], the same
//! code, so `generate` never fails on a verified program. The rules hold in both modes, so
//! switching between `ssr` and `split` never changes which programs are accepted.

use crate::ast::{Domain as AstDomain, Program};
use crate::config::{Mode, TargetConfig};
use crate::core::ir::{CoreFunction, CoreProgram, CoreType, Expr, ExprKind, Prim};
use crate::types::ty::{Type, display};

/// The base types a component parameter may have (directly or under distinct types): the
/// types with a strict text decoder in both runtimes.
pub const PARAMETER_TYPES: [&str; 8] = [
    "String", "Email", "Url", "Int", "UUID", "Date", "DateTime", "Instant",
];

/// Why a verified program or target has no web frontend (E8311).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrontendProblem {
    /// The removed Elm client.
    ElmClient { target: String },
    /// `api_url` is neither a path nor an http(s) URL.
    InvalidApiUrl { api_url: String },
    /// `api_url` is on another origin and no `frontend_origin` names the allowed one.
    CrossOriginApi { api_url: String },
    /// `frontend_origin` is not an origin.
    InvalidFrontendOrigin { origin: String },
    /// The program has no client component.
    NoComponent { target: String },
    /// A component parameter whose type a URL cannot carry.
    Parameter {
        component: String,
        name: String,
        ty: String,
    },
    /// A function reachable from the component (or the component itself) uses `what`.
    Unsupported {
        function: String,
        component: String,
        what: &'static str,
    },
    /// A function reachable from the component (or the component itself) has no body.
    Bodiless { function: String, component: String },
}

impl FrontendProblem {
    /// The function the problem is about, if any (for the diagnostic's location).
    pub fn function(&self) -> Option<&str> {
        match self {
            FrontendProblem::Parameter { component, .. } => Some(component),
            FrontendProblem::Unsupported { function, .. }
            | FrontendProblem::Bodiless { function, .. } => Some(function),
            _ => None,
        }
    }

    pub fn message(&self) -> String {
        let profile = crate::certified::profile_version();
        match self {
            FrontendProblem::ElmClient { target } => format!(
                "Target `{target}` uses `client = \"elm\"`, which is no longer supported: write `client = \"web\"` and choose `mode = \"ssr\"` (the server renders the page) or `mode = \"split\"` (static frontend calling the API)."
            ),
            FrontendProblem::InvalidApiUrl { api_url } => format!(
                "`api_url = \"{api_url}\"` must be a path starting with `/` or an `http://` or `https://` URL."
            ),
            FrontendProblem::CrossOriginApi { api_url } => format!(
                "`api_url = \"{api_url}\"` is on another origin, so the target must declare `frontend_origin`, the only browser origin the API answers (CORS)."
            ),
            FrontendProblem::InvalidFrontendOrigin { origin } => format!(
                "`frontend_origin = \"{origin}\"` must be an origin: `http://` or `https://`, a host and an optional port, without a path."
            ),
            FrontendProblem::NoComponent { target } => format!(
                "Target `{target}` has no client component: a web target renders exactly one `client component` as its page."
            ),
            FrontendProblem::Parameter {
                component,
                name,
                ty,
            } => format!(
                "Component `{component}` has parameter `{name}` of type `{ty}`, which a page URL cannot carry in profile {profile} (allowed: String, Email, Url, Int, UUID, Date, DateTime, Instant and distinct types over them)."
            ),
            FrontendProblem::Unsupported {
                function,
                component,
                what,
            } if function == component => format!(
                "Component `{component}` {what}, which the web frontend does not support in profile {profile}."
            ),
            FrontendProblem::Unsupported {
                function,
                component,
                what,
            } => format!(
                "Function `{function}` is reachable from component `{component}` but {what}, which the web frontend does not support in profile {profile}."
            ),
            FrontendProblem::Bodiless {
                function,
                component,
            } if function == component => format!(
                "Component `{component}` has no body, so the web frontend cannot run it in profile {profile}."
            ),
            FrontendProblem::Bodiless {
                function,
                component,
            } => format!(
                "Function `{function}` is reachable from component `{component}` but has no body, so the web frontend cannot run it in profile {profile}."
            ),
        }
    }
}

/// A component parameter: its name and the base type its URL text is decoded as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageParam {
    pub name: String,
    /// One of [`PARAMETER_TYPES`] (distinct types are transparent in the URL).
    pub base: String,
}

/// The page of a web target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontendPlan {
    /// The client component rendered as the page (the first one; E8301 allows only one in
    /// certified targets).
    pub component: String,
    pub params: Vec<PageParam>,
    /// Whether a function the component reaches calls the server.
    pub uses_server: bool,
}

impl FrontendPlan {
    /// A component without parameters or server calls: evaluated at compile time, when the
    /// evaluator can (see `page::prerender`).
    pub fn may_be_static(&self) -> bool {
        self.params.is_empty() && !self.uses_server
    }
}

/// Every reason why `target` (named `name`) cannot have a frontend for the program, in a
/// deterministic order: the target's own settings first, then the program's (`verify` reports
/// the two parts at different stages: [`target_problems`], [`program_problems`]).
#[cfg(test)]
pub fn frontend_problems(
    core: &CoreProgram,
    ast: &Program,
    name: &str,
    target: &TargetConfig,
) -> Vec<FrontendProblem> {
    match frontend_plan(core, ast, name, target) {
        Ok(_) => Vec::new(),
        Err(problems) => problems,
    }
}

/// The page of the program on `target`, or every reason why there is none.
pub fn frontend_plan(
    core: &CoreProgram,
    ast: &Program,
    name: &str,
    target: &TargetConfig,
) -> Result<FrontendPlan, Vec<FrontendProblem>> {
    let mut problems = target_problems(name, target);
    match program_plan(core, ast, name) {
        Ok(plan) if problems.is_empty() => Ok(plan),
        Ok(_) => Err(problems),
        Err(mut found) => {
            problems.append(&mut found);
            Err(problems)
        }
    }
}

/// The problems of the program alone (any web target): [`frontend_problems`] without
/// [`target_problems`].
pub fn program_problems(core: &CoreProgram, ast: &Program, name: &str) -> Vec<FrontendProblem> {
    program_plan(core, ast, name).err().unwrap_or_default()
}

/// The problems of the target's settings alone (no program needed).
pub fn target_problems(name: &str, target: &TargetConfig) -> Vec<FrontendProblem> {
    if target.client == "elm" {
        return vec![FrontendProblem::ElmClient {
            target: name.to_string(),
        }];
    }
    let mut problems = Vec::new();
    if target.mode == Mode::Split {
        let api_url = &target.api_url;
        let absolute = api_url.starts_with("http://") || api_url.starts_with("https://");
        let relative = api_url.starts_with('/') && !api_url.starts_with("//");
        if !absolute && !relative {
            problems.push(FrontendProblem::InvalidApiUrl {
                api_url: api_url.clone(),
            });
        } else if absolute && target.frontend_origin.is_none() {
            problems.push(FrontendProblem::CrossOriginApi {
                api_url: api_url.clone(),
            });
        }
    }
    if let Some(origin) = &target.frontend_origin
        && !is_origin(origin)
    {
        problems.push(FrontendProblem::InvalidFrontendOrigin {
            origin: origin.clone(),
        });
    }
    problems
}

/// `http://` or `https://`, then a non-empty host (with an optional port) and nothing else.
fn is_origin(text: &str) -> bool {
    let host = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("http://"));
    host.is_some_and(|host| {
        !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
    })
}

fn program_plan(
    core: &CoreProgram,
    ast: &Program,
    name: &str,
) -> Result<FrontendPlan, Vec<FrontendProblem>> {
    let Some(component) = ast
        .functions
        .iter()
        .find(|function| function.domain == AstDomain::Client && function.component)
        .and_then(|function| {
            core.functions
                .iter()
                .find(|item| item.name == function.name)
        })
    else {
        return Err(vec![FrontendProblem::NoComponent {
            target: name.to_string(),
        }]);
    };
    let mut problems = Vec::new();
    let mut params = Vec::new();
    for (_, parameter, ty) in &component.params {
        match parameter_base(ty, core, 0) {
            Some(base) => params.push(PageParam {
                name: parameter.clone(),
                base,
            }),
            None => problems.push(FrontendProblem::Parameter {
                component: component.name.clone(),
                name: parameter.clone(),
                ty: display(ty),
            }),
        }
    }
    let mut uses_server = false;
    for function in client_functions(core, &component.name) {
        let problem = |what| FrontendProblem::Unsupported {
            function: function.name.clone(),
            component: component.name.clone(),
            what,
        };
        if function.bodiless {
            problems.push(FrontendProblem::Bodiless {
                function: function.name.clone(),
                component: component.name.clone(),
            });
        }
        if contains(&function.body, &|expr| {
            matches!(expr.kind, ExprKind::Environment(_))
        }) {
            problems.push(problem("uses an `environment` value"));
        }
        if contains(&function.body, &|expr| {
            matches!(
                expr.kind,
                ExprKind::Prim {
                    op: Prim::DecimalMul | Prim::DecimalDivChecked,
                    ..
                }
            )
        }) {
            problems.push(problem("multiplies or divides `Decimal`/`Money` values"));
        }
        if contains(&function.body, &|expr| {
            matches!(
                expr.kind,
                ExprKind::Prim {
                    op: Prim::DurationAdd | Prim::DurationSub | Prim::DurationNeg,
                    ..
                }
            )
        }) {
            problems.push(problem("does arithmetic on `Duration` values"));
        }
        uses_server |= contains(&function.body, &|expr| {
            matches!(expr.kind, ExprKind::ServerCall { .. })
        });
    }
    if problems.is_empty() {
        Ok(FrontendPlan {
            component: component.name.clone(),
            params,
            uses_server,
        })
    } else {
        Err(problems)
    }
}

/// The functions `component` reaches (itself included), sorted by name.
fn client_functions<'p>(core: &'p CoreProgram, component: &str) -> Vec<&'p CoreFunction> {
    crate::core::lower::client_functions(core, component)
        .iter()
        .filter_map(|name| core.functions.iter().find(|item| item.name == *name))
        .collect()
}

/// The base type of a parameter of type `ty` (distinct types peeled), when a URL can carry it.
fn parameter_base(ty: &Type, core: &CoreProgram, depth: usize) -> Option<String> {
    let Type::Con(name, args) = ty else {
        return None;
    };
    if !args.is_empty() || depth > core.types.len() {
        return None;
    }
    for declaration in &core.types {
        match declaration {
            CoreType::Distinct {
                name: declared,
                underlying,
            } if declared == name => return parameter_base(underlying, core, depth + 1),
            CoreType::Record { name: declared, .. } | CoreType::Sum { name: declared, .. }
                if declared == name =>
            {
                return None;
            }
            _ => {}
        }
    }
    PARAMETER_TYPES
        .contains(&name.as_str())
        .then(|| name.clone())
}

/// Whether `expr` or one of its subexpressions satisfies `test`.
fn contains(expr: &Expr, test: &dyn Fn(&Expr) -> bool) -> bool {
    test(expr)
        || crate::backend::rust::children(expr)
            .into_iter()
            .any(|child| contains(child, test))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_problems_are_the_targets_then_the_programs() {
        let source = "client component C(flag: Boolean) {\n UI {\n Text(flag)\n }\n}\n";
        let core = crate::core::lower::tests::compile(source);
        let (ast, _) = crate::parser::parse(crate::lexer::lex("main.bd", source).0);
        let target = TargetConfig {
            client: "web".into(),
            mode: Mode::Split,
            api_url: "https://api.example.com/api".into(),
            ..TargetConfig::default()
        };
        let problems = frontend_problems(&core, &ast, "t", &target);
        assert_eq!(
            problems,
            vec![
                FrontendProblem::CrossOriginApi {
                    api_url: "https://api.example.com/api".into()
                },
                FrontendProblem::Parameter {
                    component: "C".into(),
                    name: "flag".into(),
                    ty: "Boolean".into()
                },
            ]
        );
        assert_eq!(
            problems,
            [
                target_problems("t", &target),
                program_problems(&core, &ast, "t")
            ]
            .concat()
        );
        // The split mode's API settings do not apply to the ssr mode.
        let ssr = TargetConfig {
            mode: Mode::Ssr,
            ..target
        };
        assert_eq!(target_problems("t", &ssr), Vec::new());
    }
}
