//! The typed core IR and its construction from the checked AST.

#[cfg(test)]
pub mod eval;
pub mod ir;
pub mod lower;
mod specialize;
pub mod validate;
