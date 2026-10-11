//! The native HTML/JS frontend (split mode). `RUNTIME` is the JS runtime, copied verbatim into
//! generated `web/` folders as `bind-runtime.js`.

/// The JS runtime (`runtime/bind-runtime-js/bind-runtime.js`).
#[cfg_attr(not(test), allow(dead_code))] // used by the JS generator
pub const RUNTIME: &str = include_str!("../../../runtime/bind-runtime-js/bind-runtime.js");

#[cfg(test)]
mod tests {
    use super::RUNTIME;

    #[test]
    fn runtime_is_embedded_and_uses_no_html_string_apis() {
        assert!(RUNTIME.contains("export const MAX_DEPTH = 10000;"));
        for forbidden in [
            "innerHTML",
            "outerHTML",
            "insertAdjacentHTML",
            "document.write",
            "eval(",
            "new Function",
        ] {
            assert!(!RUNTIME.contains(forbidden), "runtime uses {forbidden}");
        }
    }

    /// Runs `runtime/bind-runtime-js/tests/run.mjs` (the JS runtime's tests, against the shared
    /// vectors) with `node`.
    #[test]
    #[ignore = "needs node; run with `cargo test backend::js -- --ignored`"]
    fn js_runtime_tests_pass_in_node() {
        let available = std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success());
        if !available {
            eprintln!("skipped: node not found");
            return;
        }
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("runtime/bind-runtime-js/tests/run.mjs");
        let run = std::process::Command::new("node")
            .arg(&script)
            .output()
            .expect("run node");
        let stdout = String::from_utf8_lossy(&run.stdout);
        println!("{stdout}");
        assert!(
            run.status.success(),
            "JS runtime tests failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&run.stderr)
        );
    }
}
