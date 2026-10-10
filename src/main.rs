mod ast;
// Wired into server generation by Task 7; until then only its tests use it.
#[allow(dead_code)]
mod backend;
mod certified;
mod cli;
mod compiler;
mod config;
mod core;
mod diagnostic;
mod lexer;
mod parser;
mod pipeline;
mod schema;
mod syntax;
mod types;

/// Stack reserved for the compiler thread. The parser bounds expression nesting and
/// operator chains, and the passes after it recurse over trees within those bounds; this
/// reserve covers that worst case with a wide margin even in unoptimized builds.
const COMPILER_STACK: usize = 64 * 1024 * 1024;

/// Runs `work` on a thread with `COMPILER_STACK` bytes of stack.
pub(crate) fn with_compiler_stack<T: Send>(work: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(COMPILER_STACK)
            .spawn_scoped(scope, work)
            .expect("failed to start the compiler thread")
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

fn main() {
    let args = std::env::args().skip(1).collect();
    if let Err(error) = with_compiler_stack(|| cli::run(args)) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
