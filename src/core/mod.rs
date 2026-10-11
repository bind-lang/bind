//! The typed core IR and its construction from the checked AST.

// Used by the JS backend (number literals); the SSR uses the rest.
#[cfg_attr(not(test), allow(dead_code))]
pub mod display;
#[cfg(test)]
pub mod eval;
pub mod ir;
pub mod lower;
mod specialize;
pub mod validate;
