//! Rust bindings for the RON (Rusty Object Notation) tree-sitter grammar.
//!
//! The generated parser in `grammar/` is vendored unchanged from
//! <https://github.com/tree-sitter-grammars/tree-sitter-ron> at
//! 78938553b93075e638035f624973083451b29055 (MIT, see `grammar/LICENSE-MIT`).
//! Its published crate depends on tree-sitter 0.20, which can't be linked
//! alongside the tree-sitter version Zed uses.

use tree_sitter_language::LanguageFn;

unsafe extern "C" {
    fn tree_sitter_ron() -> *const ();
}

pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_ron) };

#[cfg(test)]
mod tests {
    #[test]
    fn test_can_load_grammar() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&super::LANGUAGE.into())
            .expect("Error loading RON parser");
    }
}
