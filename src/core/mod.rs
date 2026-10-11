//! The typed core IR and its construction from the checked AST.

pub mod bigint;
// The JS backend (number literals) and the static pre-render use part of these; the tests and
// the differential use the rest.
#[cfg_attr(not(test), allow(dead_code))]
pub mod display;
#[cfg_attr(not(test), allow(dead_code))]
pub mod eval;
pub mod ir;
pub mod lower;
mod specialize;
pub mod validate;
