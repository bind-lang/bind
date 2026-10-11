//! The `web/` folder of a web target: the page shell shared by both modes, the static page
//! pre-rendered at compile time, and the split mode's page (`index.html` with the loading
//! screen, `app.js` with the boot code, the runtime) — all from the [`FrontendPlan`].
//!
//! | Page | `ssr` | `split` |
//! |---|---|---|
//! | static (no parameters, no server call, evaluated by `core::eval`) | `index.html` pre-rendered, no JS | the same |
//! | dynamic | rendered by the server at `GET /`; `web/` holds the stylesheet | `index.html` + `app.js` + `bind-runtime.js` |
//!
//! `styles.css` is always written; every page links it as [`STYLESHEET`].

use crate::compiler::VerifiedApplication;
use crate::config::{Mode, ProjectConfig, TargetConfig};
use crate::core::display::{UiArg, UiTree, escape_html, render_html_value};
use crate::core::eval::{PRERENDER_BUDGET, eval_function_within};
use crate::core::ir::{CoreProgram, UiNode};

use super::names::js_string;
use super::plan::{FrontendPlan, frontend_plan};

/// The stylesheet URL of every page (the shell's `<link>`).
pub const STYLESHEET: &str = "/styles.css";

/// The text of the loading screen: the runtimes' `TEXT_LOADING` / `TEXTS.loading`.
pub const LOADING_TEXT: &str = "Loading…";

/// The name `app.js` exports its page object (`{ readParams, boot }`) under, unless the
/// component itself has that name (then `_` is appended until they differ).
pub const PAGE_EXPORT: &str = "bind:page";

/// The metadata of the page shell (the runtimes' `PageMeta` / `pageHtml`'s `meta`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageMeta<'a> {
    pub title: &'a str,
    /// No meta description / og:description lines when `None`.
    pub description: Option<&'a str>,
    pub language: &'a str,
    pub stylesheet: &'a str,
}

/// The page metadata of a project: `[web]` title, description (none when empty) and language,
/// and [`STYLESHEET`].
pub fn page_meta(config: &ProjectConfig) -> PageMeta<'_> {
    PageMeta {
        title: &config.web_title,
        description: Some(config.web_description.as_str()).filter(|text| !text.is_empty()),
        language: &config.web_language,
        stylesheet: STYLESHEET,
    }
}

/// The page icon of the shell (the runtimes' `PAGE_ICON`), written as is.
pub const PAGE_ICON: &str = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 64 64'%3E%3Crect width='64' height='64' fill='%2311130f'/%3E%3Ctext x='32' y='45' text-anchor='middle' font-family='monospace' font-size='40' font-weight='700' fill='%23c8ff36'%3EB%3C/text%3E%3C/svg%3E";

/// The single page shell of both modes (the runtimes' `page_html` / `pageHtml`): a full HTML
/// document, line by line the former generator's `index.html`, with the metadata of `meta`
/// (HTML-escaped; the description lines only when there is one) and `body` (already HTML)
/// inside the one `<div class="bind-site">`.
pub fn page_html(meta: &PageMeta, body: &str) -> String {
    let title = escape_html(meta.title);
    let description = meta.description.map(escape_html);
    let mut lines = vec![
        "<meta charset=\"utf-8\">".to_string(),
        "<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">".to_string(),
    ];
    if let Some(description) = &description {
        lines.push(format!(
            "<meta name=\"description\" content=\"{description}\">"
        ));
    }
    lines.push("<meta name=\"theme-color\" content=\"#efeee8\">".to_string());
    lines.push(format!("<meta property=\"og:title\" content=\"{title}\">"));
    if let Some(description) = &description {
        lines.push(format!(
            "<meta property=\"og:description\" content=\"{description}\">"
        ));
    }
    lines.push("<meta property=\"og:type\" content=\"website\">".to_string());
    lines.push(format!("<title>{title}</title>"));
    lines.push(format!("<link rel=\"icon\" href=\"{PAGE_ICON}\">"));
    lines.push(format!(
        "<link rel=\"stylesheet\" href=\"{}\">",
        escape_html(meta.stylesheet)
    ));
    let head: String = lines.iter().map(|line| format!("    {line}\n")).collect();
    format!(
        "<!doctype html>\n<html lang=\"{}\">\n  <head>\n{head}  </head>\n  <body>\n    <div class=\"bind-site\">{body}</div>\n  </body>\n</html>\n",
        escape_html(meta.language)
    )
}

/// The loading screen's HTML (the JS runtime's `LOADING`).
pub fn loading_html() -> String {
    let text = UiTree {
        node: UiNode::Text,
        args: vec![UiArg::Str(LOADING_TEXT.to_string())],
        children: Vec::new(),
    };
    let paragraph = UiTree {
        node: UiNode::Paragraph,
        args: Vec::new(),
        children: vec![text],
    };
    render_html_value(&UiTree {
        node: UiNode::Main,
        args: vec![UiArg::Str("bind-loading".to_string())],
        children: vec![paragraph],
    })
}

/// The HTML of the page's component evaluated at compile time, when it is static: no
/// parameters, no server call, and `core::eval` evaluates it to a UI tree within
/// `PRERENDER_BUDGET` whose HTML has at most `PRERENDER_BUDGET.html_bytes` bytes (and nodes).
/// The budget bounds the evaluation's work (operations, `Int` and string sizes, values
/// compared, UI nodes built) and the rendering's (bytes written, nodes visited: subtrees are
/// shared, so a tree that doubles at each step has an exponential HTML in linear memory), so
/// the pre-render takes bounded time and memory. Otherwise (it fails, exceeds the budget or
/// uses what the evaluator does not model) `None`: the page is rendered at run time, where a
/// failure shows its error screen with its status.
pub fn prerender(core: &CoreProgram, plan: &FrontendPlan) -> Option<String> {
    if !plan.may_be_static() {
        return None;
    }
    let value = eval_function_within(core, &plan.component, Vec::new(), PRERENDER_BUDGET).ok()?;
    value.render_html_within(PRERENDER_BUDGET.html_bytes)
}

/// The default stylesheet (no `[web].stylesheet`): the page wrapper and the standard screens.
pub const DEFAULT_CSS: &str = ":root { color-scheme: light dark; font-family: ui-sans-serif, system-ui, sans-serif; line-height: 1.5; }
body { margin: 0; }
.bind-site { max-width: 48rem; margin: 0 auto; padding: 2rem 1rem; }
.bind-loading { opacity: .7; }
.bind-error h1 { font-size: 1.5rem; margin: 0 0 .5rem; }
.bind-error p { margin: 0; }
";

/// The stylesheet of the project: `[web].stylesheet` or [`DEFAULT_CSS`].
pub fn stylesheet(config: &ProjectConfig) -> Result<String, String> {
    match &config.web_stylesheet {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|error| format!("Cannot read web stylesheet {}: {error}", path.display())),
        None => Ok(DEFAULT_CSS.to_string()),
    }
}

/// The frontend of `app` on `target`: its plan, whether the page is static (pre-rendered) and
/// the files of `web/` (relative path, content; see the module table).
#[derive(Clone, Debug)]
pub struct WebBuild {
    pub plan: FrontendPlan,
    pub is_static: bool,
    pub files: Vec<(String, String)>,
}

/// The files of `web/` for `app` on `target`; see the module table.
#[cfg(test)]
pub fn web_files(
    app: &VerifiedApplication,
    config: &ProjectConfig,
    target: &TargetConfig,
) -> Result<Vec<(String, String)>, String> {
    web_build(app, config, target).map(|build| build.files)
}

/// The [`WebBuild`] of `app` on `target`. Fails only for what `verify` reports (E8311, the
/// same plan) or an unreadable stylesheet.
pub fn web_build(
    app: &VerifiedApplication,
    config: &ProjectConfig,
    target: &TargetConfig,
) -> Result<WebBuild, String> {
    let plan = frontend_plan(&app.core, &app.program, &app.target, target).map_err(|problems| {
        problems
            .iter()
            .map(|problem| problem.message())
            .collect::<Vec<_>>()
            .join(" ")
    })?;
    let mut files = Vec::new();
    let prerendered = prerender(&app.core, &plan);
    if let Some(body) = &prerendered {
        files.push((
            "web/index.html".to_string(),
            page_html(&page_meta(config), body),
        ));
    } else if target.mode == Some(Mode::Split) {
        let export = page_export(&plan);
        files.push((
            "web/index.html".to_string(),
            split_index(&page_meta(config), &export),
        ));
        files.push((
            "web/app.js".to_string(),
            app_module(&app.core, &plan, &target.api_url, &export),
        ));
        files.push((
            "web/bind-runtime.js".to_string(),
            super::RUNTIME.to_string(),
        ));
    }
    files.push(("web/styles.css".to_string(), stylesheet(config)?));
    Ok(WebBuild {
        plan,
        is_static: prerendered.is_some(),
        files,
    })
}

fn page_export(plan: &FrontendPlan) -> String {
    let mut export = PAGE_EXPORT.to_string();
    while export == plan.component {
        export.push('_');
    }
    export
}

/// The split page before its JS runs: the shell with the loading screen, then the module
/// script that boots the page into the shell's `div.bind-site`.
fn split_index(meta: &PageMeta, export: &str) -> String {
    let page = page_html(meta, &loading_html());
    let script = format!(
        "    <script type=\"module\">import {{ {} as page }} from \"/app.js\"; page.boot(document.querySelector(\"div.bind-site\"), window.location.search, document);</script>\n",
        js_string(export)
    );
    match page.strip_suffix("  </body>\n</html>\n") {
        Some(start) => format!("{start}{script}  </body>\n</html>\n"),
        None => page,
    }
}

/// `app.js` of the split page: the component's module (`render_app_with`) and the page object
/// exported as `export`: `readParams(search)` reads the component's parameters with the strict
/// text decoders (absent, repeated or invalid: `BindError("BadParameter", name)`, as the SSR's
/// 400 page) and `boot(container, search, document)` shows the loading screen, runs the
/// component and renders its UI into `container`, or `rt.errorScreen` of any failure (reading
/// the parameters, running the component or rendering its UI).
pub fn app_module(core: &CoreProgram, plan: &FrontendPlan, api_url: &str, export: &str) -> String {
    let module = super::render_app_with(core, std::slice::from_ref(&plan.component), api_url);
    let specs: Vec<String> = plan
        .params
        .iter()
        .map(|param| {
            let decoder = match param.base.as_str() {
                "Int" => "textInt",
                "UUID" => "textUuid",
                "Date" => "textDate",
                "DateTime" => "textDateTime",
                "Instant" => "textInstant",
                _ => "textString",
            };
            format!("[{}, rt.{decoder}]", js_string(&param.name))
        })
        .collect();
    let mut arguments = vec!["rt.newCtx()".to_string()];
    arguments.extend(
        plan.params
            .iter()
            .map(|param| format!("params[{}]", js_string(&param.name))),
    );
    let boot = format!(
        "
// The page (split mode): reads the component's parameters from the query string, shows the
// loading screen, runs the component and renders its UI, or the error screen of the failure,
// into the page's `div.bind-site` (index.html calls `boot`).
function readPageParams(search) {{
return rt.readParams(search, [{specs}]);
}}

async function bootPage(container, search, document) {{
rt.mount(container, rt.render(rt.LOADING, document));
try {{
const params = readPageParams(search);
const tree = await {component}({arguments});
rt.mount(container, rt.render(tree, document));
}} catch (error) {{
// Any failure, rendering included, shows its error screen (never a page stuck loading).
rt.mount(container, rt.render(rt.errorScreen(error).tree, document));
}}
}}

const page = Object.freeze({{ readParams: readPageParams, boot: bootPage }});
export {{ page as {export} }};
",
        specs = specs.join(", "),
        component = super::names::js_value(&plan.component),
        arguments = arguments.join(", "),
        export = js_string(export),
    );
    crate::backend::rust::indent(&format!("{module}{boot}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_matches_the_runtimes_page_vectors() {
        let vectors = include_str!("../../../runtime/vectors/pages.json");
        let json = |html: &str| {
            html.replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
        };
        for (title, description, language, stylesheet, body) in [
            ("Home", None, "en", "/styles.css", "<p>hi</p>"),
            (
                "A & B <i>",
                Some("Uma \"loja\" & <tudo> 'aqui'"),
                "pt-BR",
                "/s.css?x=\"1\"&y='2'",
                "<main>x</main>",
            ),
            ("", Some(""), "", "", ""),
        ] {
            let meta = PageMeta {
                title,
                description,
                language,
                stylesheet,
            };
            let html = page_html(&meta, body);
            assert!(vectors.contains(&json(&html)), "{html}");
        }
        let runtime = include_str!("../../../runtime/bind-runtime/src/lib.rs");
        assert!(runtime.contains(&format!("pub const PAGE_ICON: &str = \"{PAGE_ICON}\";")));
    }

    #[test]
    fn the_loading_screen_uses_the_runtimes_text() {
        let runtime = include_str!("../../../runtime/bind-runtime/src/lib.rs");
        assert!(runtime.contains(&format!(
            "pub const TEXT_LOADING: &str = \"{LOADING_TEXT}\";"
        )));
        assert_eq!(
            loading_html(),
            "<main class=\"bind-loading\"><p>Loading…</p></main>"
        );
    }

    #[test]
    fn the_split_page_is_the_shell_with_the_loading_screen_and_the_boot_script() {
        let meta = PageMeta {
            title: "Shop & co",
            description: None,
            language: "pt-BR",
            stylesheet: STYLESHEET,
        };
        let page = split_index(&meta, "bind:page");
        let shell = page_html(&meta, &loading_html());
        assert_eq!(
            page,
            shell.replace(
                "  </body>",
                "    <script type=\"module\">import { \"bind:page\" as page } from \"/app.js\"; page.boot(document.querySelector(\"div.bind-site\"), window.location.search, document);</script>\n  </body>"
            )
        );
        assert!(page.contains("<html lang=\"pt-BR\">"));
        assert!(page.contains(
            "<div class=\"bind-site\"><main class=\"bind-loading\"><p>Loading…</p></main></div>"
        ));
    }

    fn plan(source: &str) -> (CoreProgram, FrontendPlan) {
        let core = crate::core::lower::tests::compile(source);
        let component = core
            .functions
            .iter()
            .find(|function| function.domain == crate::core::ir::Domain::Client)
            .unwrap();
        let params = component
            .params
            .iter()
            .map(|(_, name, ty)| super::super::plan::PageParam {
                name: name.clone(),
                base: crate::types::ty::display(ty),
            })
            .collect();
        let plan = FrontendPlan {
            component: component.name.clone(),
            params,
            uses_server: false,
        };
        (core, plan)
    }

    #[test]
    fn static_components_are_prerendered_when_the_evaluator_can() {
        let (core, page) = plan(
            "fun nome(): String = \"<Bind>\"\nclient component C() {\n UI {\n Section(\"hero\") { Heading(1) { Text(nome()) } }\n }\n}\n",
        );
        assert_eq!(
            prerender(&core, &page).as_deref(),
            Some("<section class=\"hero\"><h1>&lt;Bind&gt;</h1></section>")
        );
        // Recursion past the limit fails at run time: rendered there, with its error screen.
        let (core, page) = plan(
            "fun fundo(n: Int): Int = if (n == 0) 0 else fundo(n - 1) + 1\nclient component C() {\n UI {\n Text(fundo(20000))\n }\n}\n",
        );
        assert_eq!(prerender(&core, &page), None);
        // Work past the pre-render budget is left to run time instead of hanging `generate`:
        // exponential recursion, and values that double at each step.
        let started = std::time::Instant::now();
        let (core, page) = plan(
            "fun fib(n: Int): Int = if (n < 2) n else fib(n - 1) + fib(n - 2)\nclient component C() {\n UI {\n Text(fib(40))\n }\n}\n",
        );
        assert_eq!(prerender(&core, &page), None);
        let (core, page) = plan(
            "fun dobra(s: String, n: Int): String = if (n == 0) s else dobra(s + s, n - 1)\nfun quadrado(x: Int, n: Int): Int = if (n == 0) x else quadrado(x * x, n - 1)\nclient component C() {\n UI {\n Text(dobra(\"ab\", 40))\n Text(quadrado(3, 40))\n }\n}\n",
        );
        assert_eq!(prerender(&core, &page), None);
        let elapsed = started.elapsed();
        assert!(elapsed < std::time::Duration::from_secs(20), "{elapsed:?}");
        // A parameter: read from the URL at run time.
        let (core, page) = plan("client component C(n: Int) {\n UI {\n Text(n)\n }\n}\n");
        assert_eq!(prerender(&core, &page), None);
    }

    /// A component whose tree doubles at each `val` (`aN = Div { aN-1; aN-1 }`): `levels`
    /// steps of evaluation, a tree of 2^levels leaves.
    fn doubling(levels: usize) -> String {
        let mut source = String::from("client component C() {\n    val a0 = UI { Text(\"x\") }\n");
        for level in 1..=levels {
            source.push_str(&format!(
                "    val a{level} = UI {{ Div {{ a{}; a{} }} }}\n",
                level - 1,
                level - 1
            ));
        }
        source.push_str(&format!("    UI {{ a{levels} }}\n}}\n"));
        source
    }

    /// A component rendering a tree `20 * n` levels deep: `n` nested calls, each adding 20
    /// nested `Div`s.
    fn deep(n: usize) -> String {
        let open = "Div { ".repeat(20);
        let close = " }".repeat(20);
        format!(
            "fun t(n: Int): Ui = if (n == 0) UI {{ Text(\"x\") }} else UI {{ {open}t(n - 1){close} }}\nclient component C() {{\n    UI {{ t({n}) }}\n}}\n"
        )
    }

    #[test]
    fn shared_subtrees_and_deep_trees_fall_back_to_run_time_rendering() {
        // Small: pre-rendered, the shared subtree written each time it occurs.
        let (core, page) = plan(&doubling(2));
        assert_eq!(
            prerender(&core, &page).as_deref(),
            Some("<div><div>xx</div><div>xx</div></div>")
        );
        // 22 and 30 levels: a few steps of evaluation, but 4 million and a billion leaves
        // (about 50 MB and 13 GB of HTML): past the HTML budget, so rendered at run time,
        // quickly and in little memory.
        for levels in [22, 30] {
            let started = std::time::Instant::now();
            let (core, page) = plan(&doubling(levels));
            assert_eq!(prerender(&core, &page), None, "{levels} levels");
            let elapsed = started.elapsed();
            assert!(
                elapsed < std::time::Duration::from_secs(10),
                "{levels}: {elapsed:?}"
            );
        }
        // 20 levels of 20 nested `Div`s are pre-rendered; 9,990 levels (about 200,000 nested
        // elements) exceed the budget and are rendered at run time, without overflowing the
        // stack (the renderers and the value's drop are iterative).
        let (core, page) = plan(&deep(20));
        let html = prerender(&core, &page).expect("pre-rendered");
        assert_eq!(
            html,
            format!("{}x{}", "<div>".repeat(400), "</div>".repeat(400))
        );
        let started = std::time::Instant::now();
        let (core, page) = plan(&deep(9990));
        assert_eq!(prerender(&core, &page), None);
        assert!(started.elapsed() < std::time::Duration::from_secs(30));
    }

    #[test]
    fn the_page_object_reads_parameters_and_boots_the_component() {
        let (core, page) =
            plan("client component C(id: UUID, nome: String) {\n UI {\n Text(nome)\n }\n}\n");
        let module = app_module(&core, &page, "/api", "bind:page");
        assert!(module.contains(
            "return rt.readParams(search, [[\"id\", rt.textUuid], [\"nome\", rt.textString]]);"
        ));
        assert!(module.contains(
            "const tree = await u_C(rt.newCtx(), params[\"id\"], params[\"nome\"]);\n        rt.mount(container, rt.render(tree, document));\n    } catch (error) {"
        ), "{module}");
        assert!(
            module
                .contains("rt.mount(container, rt.render(rt.errorScreen(error).tree, document));")
        );
        assert!(
            module.ends_with("export { page as \"bind:page\" };\n"),
            "{module}"
        );
        let named = FrontendPlan {
            component: "bind:page".to_string(),
            ..page
        };
        assert_eq!(page_export(&named), "bind:page_");
    }

    /// A verified project in a temporary directory, removed on drop.
    struct Project {
        root: std::path::PathBuf,
        config: ProjectConfig,
        app: VerifiedApplication,
    }

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let path = entry.unwrap().path();
            let target = to.join(path.file_name().unwrap());
            if path.is_dir() {
                copy_tree(&path, &target);
            } else {
                std::fs::copy(&path, &target).unwrap();
            }
        }
    }

    /// `files` (relative path, content) written to a fresh temporary project, verified on `target`.
    fn project(name: &str, files: &[(&str, String)], target: &str) -> Project {
        let root = std::env::temp_dir().join(format!("bindc-page-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (path, content) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        let config = ProjectConfig::load(&root.join("bind.toml")).unwrap();
        let result = crate::compiler::verify(&config, target);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let app = result.application.unwrap();
        Project { root, config, app }
    }

    /// The site (`../website`, not modified: a temporary copy with `client = "web"` and
    /// `mode = "split"`) pre-renders exactly the HTML its former Elm client rendered
    /// (`tests/codegen/website.html.expected`, recorded before the Elm generator was removed),
    /// in the former page document.
    #[test]
    fn the_website_prerenders_the_html_of_the_former_elm_client() {
        let site = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../website");
        if !site.join("src/site.bd").exists() {
            eprintln!("skipped: ../website not found");
            return;
        }
        let manifest = std::fs::read_to_string(site.join("bind.toml"))
            .unwrap()
            .replace("client = \"elm\"", "client = \"web\"\nmode = \"split\"");
        assert!(manifest.contains("mode = \"split\""));
        let root = std::env::temp_dir().join(format!("bindc-page-website-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        copy_tree(&site.join("src"), &root.join("src"));
        std::fs::copy(site.join("styles.css"), root.join("styles.css")).unwrap();
        std::fs::write(root.join("bind.toml"), &manifest).unwrap();
        let config = ProjectConfig::load(&root.join("bind.toml")).unwrap();
        let result = crate::compiler::verify(&config, "vercel");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let app = result.application.unwrap();
        let files = web_files(&app, &config, &config.targets["vercel"]).unwrap();
        let paths: Vec<&str> = files.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(paths, ["web/index.html", "web/styles.css"]);
        let expected = include_str!("../../../tests/codegen/website.html.expected");
        let inner = expected
            .strip_prefix("<div class=\"bind-site\">")
            .and_then(|rest| rest.strip_suffix("</div>"))
            .unwrap();
        // The whole document is the former generator's `index.html` for the site
        // (`tests/codegen/website.index.html.expected`, recorded with the generator of the
        // parent of db5c316), except for the one documented difference: the Elm mount point
        // and its two scripts become the pre-rendered page in the `bind-site` wrapper.
        let elm_mount = "<div id=\"app\"></div>\n    <script src=\"/main.js\"></script>\n    <script>Elm.Main.init({ node: document.getElementById('app') });</script>";
        let former = include_str!("../../../tests/codegen/website.index.html.expected");
        assert!(former.contains(elm_mount));
        assert_eq!(
            files[0].1,
            former.replace(
                elm_mount,
                &format!("<div class=\"bind-site\">{inner}</div>")
            )
        );
        assert_eq!(files[0].1, page_html(&page_meta(&config), inner));
        assert_eq!(
            files[1].1,
            std::fs::read_to_string(site.join("styles.css")).unwrap()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn node_available() -> bool {
        std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    const SCHEMA: &str = "table,model,primary_key,column,type,nullable,required,has_default,engine\ncustomers,Customer,id,id,CustomerId,false,true,true,postgresql\ncustomers,Customer,id,name,String,false,true,false,postgresql\n";

    fn split_project(name: &str, source: &str) -> Project {
        project(
            name,
            &[
                ("app/main.bd", source.to_string()),
                ("schema.csv", SCHEMA.to_string()),
                (
                    "bind.toml",
                    "[project]\nname = \"t\"\nsource = \"app\"\n[database.main]\nschema = \"schema.csv\"\nverification = \"static\"\n[target.t]\nclient = \"web\"\nserver = \"rust\"\ndeployment = \"vercel\"\nmode = \"split\"\ncapabilities = [\"ui.dom\", \"server.http\", \"db.read\"]\n".to_string(),
                ),
            ],
            "t",
        )
    }

    /// `generate` of the pathological pages: fast, with the page rendered at run time.
    #[test]
    fn generating_huge_or_deep_static_pages_falls_back_to_the_split_page() {
        for (name, source) in [
            ("doubling-22", doubling(22)),
            ("doubling-30", doubling(30)),
            ("deep", deep(9990)),
        ] {
            let started = std::time::Instant::now();
            let project = split_project(name, &source);
            let build =
                web_build(&project.app, &project.config, &project.config.targets["t"]).unwrap();
            assert!(!build.is_static, "{name}");
            let paths: Vec<&str> = build.files.iter().map(|(path, _)| path.as_str()).collect();
            assert_eq!(
                paths,
                [
                    "web/index.html",
                    "web/app.js",
                    "web/bind-runtime.js",
                    "web/styles.css"
                ],
                "{name}"
            );
            assert!(
                build
                    .files
                    .iter()
                    .all(|(_, content)| content.len() < 1_000_000)
            );
            let elapsed = started.elapsed();
            assert!(
                elapsed < std::time::Duration::from_secs(30),
                "{name}: {elapsed:?}"
            );
        }
    }

    /// The `ssr` server of a page whose tree is about 200,000 elements deep (not pre-rendered,
    /// so rendered at `GET /`), compiled into a crate whose test serves the page: 200 with the
    /// whole tree, no stack overflow while rendering it or dropping it.
    #[test]
    #[ignore = "builds a Rust crate; run with `cargo test backend::js -- --ignored`"]
    fn the_ssr_server_renders_a_deep_tree() {
        let project = project(
            "ssr-deep",
            &[
                ("app/main.bd", deep(9990)),
                ("schema.csv", SCHEMA.to_string()),
                (
                    "bind.toml",
                    "[project]\nname = \"t\"\nsource = \"app\"\n[database.main]\nschema = \"schema.csv\"\nverification = \"static\"\n[target.t]\nclient = \"web\"\nserver = \"rust\"\ndeployment = \"aws-lambda\"\nmode = \"ssr\"\ncapabilities = [\"ui.dom\", \"server.http\", \"db.read\"]\n".to_string(),
                ),
            ],
            "t",
        );
        let output = project.root.join("generated");
        crate::certified::generate(&project.app, &project.config, Some(&output)).unwrap();
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let crate_dir = crate::backend::rust::tests::test_crate_in(
            &root.join("target/bind-codegen-tests"),
            "bind-ssr-deep",
        );
        std::fs::copy(output.join("src/lib.rs"), crate_dir.join("src/lib.rs")).unwrap();
        std::fs::copy(
            output.join("src/bind_runtime.rs"),
            crate_dir.join("src/bind_runtime.rs"),
        )
        .unwrap();
        std::fs::create_dir_all(crate_dir.join("tests")).unwrap();
        std::fs::write(
            crate_dir.join("tests/deep.rs"),
            r#"#![allow(clippy::unwrap_used)]

#[test]
fn the_deep_page_renders() {
    // The runtime of the generated entry points: 64 MiB worker stacks for deep Bind recursion.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_stack_size(64 * 1024 * 1024)
        .build()
        .unwrap();
    let pool = runtime.block_on(async {
        sqlx::postgres::PgPoolOptions::new().connect_lazy("postgresql://bind-test.invalid/none")
    }).unwrap();
    let page = runtime.block_on(bind_ssr_deep::serve_page(String::new(), pool));
    assert_eq!(page.status, 200, "{}", &page.body[..page.body.len().min(500)]);
    let tree = format!("{}x{}", "<div>".repeat(199_800), "</div>".repeat(199_800));
    assert!(page.body.contains(&format!("<div class=\"bind-site\">{tree}</div>")));
}
"#,
        )
        .unwrap();
        crate::backend::rust::tests::cargo(
            &crate_dir,
            &[
                "clippy",
                "--quiet",
                "--release",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ],
        );
        crate::backend::rust::tests::cargo(&crate_dir, &["test", "--quiet", "--release"]);
    }

    /// Review Focus 1 in the split mode: the generated `app.js` reads its parameters with
    /// `rt.readParams` as the SSR does (`%XX` and `+` decoded; absent, repeated or invalid is
    /// `BadParameter`), and `boot` renders the component or the SSR's error screen.
    #[test]
    #[ignore = "needs node; run with `cargo test backend::js -- --ignored`"]
    fn the_split_page_reads_its_parameters_like_the_ssr() {
        if !node_available() {
            eprintln!("skipped: node not found");
            return;
        }
        let customer = split_project(
            "node-customer",
            "distinct type CustomerId = UUID\nentity Customer(val id: CustomerId, val name: String)\nserver fun getCustomer(id: CustomerId): Customer effects [db.read] = db.customers.require(id)\nclient component CustomerPage(id: CustomerId) {\n    val customer = server.getCustomer(id)\n    UI { Text(customer.name) }\n}\n",
        );
        let greeting = split_project(
            "node-greeting",
            "client component Ola(nome: String) {\n    UI { Paragraph { Text(\"Olá, \" + nome) } }\n}\n",
        );
        let folder = std::env::temp_dir().join(format!("bindc-page-node-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        for (project, directory) in [(&customer, "customer"), (&greeting, "greeting")] {
            let target = &project.config.targets["t"];
            for (path, content) in web_files(&project.app, &project.config, target).unwrap() {
                let path = folder.join(directory).join(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, content).unwrap();
            }
        }
        std::fs::write(
            folder.join("dom.mjs"),
            include_str!("../../../runtime/bind-runtime-js/tests/dom.mjs"),
        )
        .unwrap();
        std::fs::write(
            folder.join("check.mjs"),
            r#"
import { "bind:page" as customer } from "./customer/web/app.js";
import { "bind:page" as greeting } from "./greeting/web/app.js";
import { createDocument, serialize } from "./dom.mjs";
const show = (read) => {
  try {
    return JSON.stringify({ ...read() });
  } catch (error) {
    // Each page has its own copy of the runtime, hence its own BindError class.
    return error?.name === "BindError" ? `${error.kind}:${error.message}` : `exception ${error}`;
  }
};
const uuid = "1b4e28ba-2fa1-4d3b-a3f5-ef19ae7a0c12";
const lines = [
  show(() => customer.readParams(`?id=${uuid}`)),
  show(() => customer.readParams("?id=a&id=b")),
  show(() => customer.readParams(`?id=${uuid}&id=${uuid}`)),
  show(() => customer.readParams("?outro=1")),
  show(() => customer.readParams("?id=1B4E28BA-2FA1-4D3B-A3F5-EF19AE7A0C12")),
  show(() => greeting.readParams("?nome=Jo%C3%A3o+Silva")),
  show(() => greeting.readParams("?nome=%E0%A4%A")),
];
const boot = async (page, search) => {
  const document = createDocument();
  const container = document.createElement("div");
  await page.boot(container, search, document);
  return container.childNodes.map(serialize).join("");
};
const requests = [];
globalThis.fetch = async (url, init) => {
  requests.push(`${url} ${init.body}`);
  const id = JSON.parse(init.body).id;
  if (id === uuid) return new Response(JSON.stringify({ id, name: "Ana <admin>" }), { status: 200 });
  return new Response('{"error":"not found"}', { status: 404 });
};
lines.push(await boot(customer, `?id=${uuid}`));
lines.push(await boot(customer, "?id=00000000-0000-4000-8000-000000000000"));
lines.push(await boot(customer, "?id=a&id=b"));
lines.push(await boot(greeting, "?nome=Jo%C3%A3o+Silva"));
// A failure while rendering the component's tree (a RangeError, say) shows the error screen,
// never a page stuck on the loading screen.
const broken = async (page, search) => {
  const document = createDocument();
  const createElement = document.createElement;
  let paragraphs = 0;
  document.createElement = (name) => {
    if (name === "p" && ++paragraphs === 2) throw new RangeError("Maximum call stack size exceeded");
    return createElement(name);
  };
  const container = document.createElement("div");
  await page.boot(container, search, document);
  return container.childNodes.map(serialize).join("");
};
lines.push(await broken(greeting, "?nome=Ana"));
lines.push(...requests);
console.log(lines.join("\n"));
"#,
        )
        .unwrap();
        let run = std::process::Command::new("node")
            .arg("check.mjs")
            .current_dir(&folder)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&run.stdout).to_string();
        assert!(
            run.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&run.stderr)
        );
        let error = |title: &str, message: &str| {
            format!("<main class=\"bind-error\"><h1>{title}</h1><p>{message}</p></main>")
        };
        let expected = [
            "{\"id\":\"1b4e28ba-2fa1-4d3b-a3f5-ef19ae7a0c12\"}".to_string(),
            "BadParameter:id".to_string(),
            "BadParameter:id".to_string(),
            "BadParameter:id".to_string(),
            "BadParameter:id".to_string(),
            "{\"nome\":\"João Silva\"}".to_string(),
            "BadParameter:nome".to_string(),
            "Ana &lt;admin&gt;".to_string(),
            error("Page not found", "not found"),
            error("Bad request", "invalid parameter: id"),
            "<p>Olá, João Silva</p>".to_string(),
            error("Something went wrong", "internal error"),
            "/api/getCustomer {\"id\":\"1b4e28ba-2fa1-4d3b-a3f5-ef19ae7a0c12\"}".to_string(),
            "/api/getCustomer {\"id\":\"00000000-0000-4000-8000-000000000000\"}".to_string(),
        ];
        assert_eq!(stdout.lines().collect::<Vec<_>>(), expected);
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
