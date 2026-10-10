mod ast;
mod certified;
mod cli;
mod compiler;
mod config;
mod diagnostic;
mod lexer;
mod parser;
mod pipeline;
mod schema;
mod syntax;

fn main() {
    if let Err(error) = cli::run(std::env::args().skip(1).collect()) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
