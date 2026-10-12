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

/// The `what` of [`FrontendProblem::Unsupported`] for a read of an `environment` value.
pub const ENVIRONMENT_PROBLEM: &str = "uses an `environment` value";

/// Why a verified program or target has no web frontend (E8311).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrontendProblem {
    /// The removed Elm client.
    ElmClient { target: String },
    /// A web target without `mode`.
    MissingMode { target: String },
    /// `api_url` is not `/api` nor a canonical origin followed by `/api`.
    InvalidApiUrl { api_url: String },
    /// `api_url` is on another origin and no `frontend_origin` names the allowed one.
    CrossOriginApi { api_url: String },
    /// `frontend_origin` is not a canonical origin.
    InvalidFrontendOrigin { origin: String },
    /// `frontend_origin` without an API on another origin (the `ssr` mode, or `api_url` on the
    /// page's own origin): there is no cross-origin call to allow.
    UnusedFrontendOrigin { target: String, origin: String },
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
            FrontendProblem::MissingMode { target } => format!(
                "Target `{target}` must choose its mode: `mode = \"ssr\"` (the server renders the page) or `mode = \"split\"` (static frontend calling the API)."
            ),
            FrontendProblem::InvalidApiUrl { api_url } => format!(
                "`api_url = \"{api_url}\"` must be `/api` or a canonical origin followed by `/api` (`https://host[:port]/api`, no trailing slash, query or fragment): the generated server answers at `/api/<endpoint>`."
            ),
            FrontendProblem::CrossOriginApi { api_url } => format!(
                "`api_url = \"{api_url}\"` is on another origin, so the target must declare `frontend_origin`, the only browser origin the API answers (CORS)."
            ),
            FrontendProblem::InvalidFrontendOrigin { origin } => format!(
                "`frontend_origin = \"{origin}\"` must be a canonical origin: `http://` or `https://`, a lowercase host and an optional non-default port, without a path, user info or trailing slash."
            ),
            FrontendProblem::UnusedFrontendOrigin { target, origin } => format!(
                "`frontend_origin = \"{origin}\"` only applies when the API is on another origin (`mode = \"split\"` with `api_url = \"https://host[:port]/api\"`), but target `{target}` serves its API from the page's own origin, so the generated server answers no CORS: remove `frontend_origin`, or set `api_url` to the API's absolute URL."
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
    match target.mode {
        None => problems.push(FrontendProblem::MissingMode {
            target: name.to_string(),
        }),
        Some(Mode::Split) => {
            let api_url = &target.api_url;
            match api_url.strip_suffix("/api") {
                Some("") => {}
                Some(origin) if is_canonical_origin(origin) => {
                    if target.frontend_origin.is_none() {
                        problems.push(FrontendProblem::CrossOriginApi {
                            api_url: api_url.clone(),
                        });
                    }
                }
                _ => problems.push(FrontendProblem::InvalidApiUrl {
                    api_url: api_url.clone(),
                }),
            }
        }
        Some(Mode::Ssr) => {}
    }
    if let Some(origin) = &target.frontend_origin {
        if !is_canonical_origin(origin) {
            problems.push(FrontendProblem::InvalidFrontendOrigin {
                origin: origin.clone(),
            });
        } else if target.mode == Some(Mode::Ssr)
            || (target.mode == Some(Mode::Split) && target.api_url == "/api")
        {
            // (An invalid `api_url` is reported on its own.)
            problems.push(FrontendProblem::UnusedFrontendOrigin {
                target: name.to_string(),
                origin: origin.clone(),
            });
        }
    }
    problems
}

/// Whether the target's API is on another origin than its page: the `split` mode with an
/// absolute `api_url` (a canonical origin followed by `/api`).
fn has_cross_origin_api(target: &TargetConfig) -> bool {
    target.mode == Some(Mode::Split)
        && target
            .api_url
            .strip_suffix("/api")
            .is_some_and(|origin| !origin.is_empty() && is_canonical_origin(origin))
}

/// The origin the generated API answers with CORS: `frontend_origin`, only when the API is on
/// another origin (`verify` rejects `frontend_origin` otherwise, E8311). `None`: no CORS.
pub fn cors_origin(target: &TargetConfig) -> Option<&str> {
    target
        .frontend_origin
        .as_deref()
        .filter(|_| has_cross_origin_api(target))
}

/// An origin as browsers write it in the `Origin` header: `http://` or `https://`, a lowercase
/// host (DNS labels of `a-z`, `0-9` and inner `-` whose last label is not a number, a
/// dotted-decimal IPv4 address of four octets 0..=255 without leading zeros, or a bracketed IPv6
/// address) and an optional port that is not the scheme's default, with nothing after it.
fn is_canonical_origin(text: &str) -> bool {
    let (rest, default_port) = if let Some(rest) = text.strip_prefix("https://") {
        (rest, "443")
    } else if let Some(rest) = text.strip_prefix("http://") {
        (rest, "80")
    } else {
        return false;
    };
    let (host, port) = if let Some(inner) = rest.strip_prefix('[') {
        let Some((address, after)) = inner.split_once(']') else {
            return false;
        };
        let valid = !address.is_empty()
            && address
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, 'a'..='f' | ':' | '.'));
        if !valid {
            return false;
        }
        match after {
            "" => ("ipv6", None),
            _ => match after.strip_prefix(':') {
                Some(port) => ("ipv6", Some(port)),
                None => return false,
            },
        }
    } else {
        match rest.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (rest, None),
        }
    };
    let label = |label: &str| {
        !label.is_empty()
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    };
    let port_ok = port.is_none_or(|port| {
        !port.is_empty()
            && port.len() <= 5
            && !port.starts_with('0')
            && port.chars().all(|c| c.is_ascii_digit())
            && port.parse::<u32>().is_ok_and(|value| value <= 65535)
            && port != default_port
    });
    if !port_ok {
        return false;
    }
    if host == "ipv6" {
        return true;
    }
    let labels: Vec<&str> = host.split('.').collect();
    // A host whose last label is a number (decimal, or `0x` hex) is an IPv4 address to a
    // browser's URL parser, which rewrites it in dotted decimal (`http://1` is
    // `http://0.0.0.1`) or rejects it: only that dotted-decimal form is canonical.
    let last = labels.last().copied().unwrap_or_default();
    let numeric = (!last.is_empty() && last.chars().all(|c| c.is_ascii_digit()))
        || last
            .strip_prefix("0x")
            .is_some_and(|hex| hex.chars().all(|c| c.is_ascii_hexdigit()));
    if numeric {
        let octet = |label: &&str| {
            !label.is_empty()
                && label.chars().all(|c| c.is_ascii_digit())
                && (label.len() == 1 || !label.starts_with('0'))
                && label.parse::<u16>().is_ok_and(|value| value <= 255)
        };
        return labels.len() == 4 && labels.iter().all(octet);
    }
    labels.iter().all(|item| label(item))
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
            problems.push(problem(ENVIRONMENT_PROBLEM));
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
            mode: Some(Mode::Split),
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
            mode: Some(Mode::Ssr),
            ..target
        };
        assert_eq!(target_problems("t", &ssr), Vec::new());
    }
}
