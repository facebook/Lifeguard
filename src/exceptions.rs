/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! What the enclosing `try` blocks catch: handler matching (`TryHandler`) and
//! the stack of blocks enclosing the node under traversal (`BlockStack`).

use pyrefly_python::module_name::ModuleName;

/// What a single `except` clause catches.
///
/// TODO: Exception names are currently matched as written in the source (e.g. "ValueError") rather
/// than fully qualified (e.g. "builtins.ValueError"). Both the handler and raise sides use
/// `full_name()` directly, so they are consistent with each other. Mismatches from mixed import
/// styles (e.g. `except ValueError` vs `raise builtins.ValueError`) produce false positives (extra
/// Raise effects), not false negatives, so this is safe. Revisit if we see false positives from
/// exception matching in practice.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum TryHandler {
    /// Bare `except:` — catches everything.
    Bare,
    /// `except SomeError:` — catches a single named type.
    Single(ModuleName),
    /// `except (A, B):` — catches multiple named types.
    /// An empty vec (from an unresolvable type expression) catches nothing.
    Multiple(Vec<ModuleName>),
}

/// Well-known exception base classes that act as catch-alls.
const CATCH_ALL_EXCEPTIONS: &[&str] = &["Exception", "BaseException"];

fn exception_matches(name: &ModuleName, exc_name: &ModuleName) -> bool {
    name == exc_name || CATCH_ALL_EXCEPTIONS.contains(&name.as_str())
}

impl TryHandler {
    pub fn typed(names: Vec<ModuleName>) -> Self {
        match <[ModuleName; 1]>::try_from(names) {
            Ok([single]) => Self::Single(single),
            Err(names) => Self::Multiple(names),
        }
    }

    pub fn catches(&self, exc_name: &ModuleName) -> bool {
        match self {
            Self::Bare => true,
            Self::Single(n) => exception_matches(n, exc_name),
            Self::Multiple(names) => names.iter().any(|n| exception_matches(n, exc_name)),
        }
    }

    /// Whether the handler names `exc_name` itself. Unlike [`Self::catches`], a
    /// catch-all does not count: naming the error says the code expected it.
    pub fn names(&self, exc_name: &ModuleName) -> bool {
        match self {
            Self::Bare => false,
            Self::Single(n) => n == exc_name,
            Self::Multiple(names) => names.contains(exc_name),
        }
    }
}

/// A kind of block that encloses an AST node.
#[derive(Debug, Clone)]
pub enum Block {
    TryBody(Vec<TryHandler>),
}

#[derive(Debug, Clone)]
pub struct BlockStack {
    stack: Vec<Block>,
}

impl BlockStack {
    pub fn new() -> Self {
        BlockStack { stack: vec![] }
    }

    pub fn in_try_body(&self) -> bool {
        self.stack.iter().any(|s| matches!(s, Block::TryBody(_)))
    }

    /// Check whether any enclosing try block has a handler that catches `exc_name`.
    pub fn catches_exception(&self, exc_name: &ModuleName) -> bool {
        self.stack.iter().rev().any(|block| match block {
            Block::TryBody(handlers) => handlers.iter().any(|h| h.catches(exc_name)),
        })
    }

    /// Whether an enclosing try block names `exc_name` itself; a catch-all does not.
    pub fn names_exception(&self, exc_name: &ModuleName) -> bool {
        self.stack.iter().rev().any(|block| match block {
            Block::TryBody(handlers) => handlers.iter().any(|h| h.names(exc_name)),
        })
    }

    /// Iterate over all try handlers from enclosing try blocks.
    pub fn try_handlers(&self) -> impl Iterator<Item = &TryHandler> + '_ {
        self.stack.iter().flat_map(|block| match block {
            Block::TryBody(handlers) => handlers.iter(),
        })
    }

    pub fn push(&mut self, block: Block) {
        self.stack.push(block);
    }

    pub fn pop(&mut self) {
        self.stack.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_try_handler_bare_catches_everything() {
        let handler = TryHandler::Bare;
        assert!(handler.catches(&ModuleName::from_str("ValueError")));
        assert!(handler.catches(&ModuleName::from_str("Whatever")));
    }

    #[test]
    fn test_try_handler_typed_exact_match() {
        let handler = TryHandler::typed(vec![ModuleName::from_str("ValueError")]);
        assert!(handler.catches(&ModuleName::from_str("ValueError")));
        assert!(!handler.catches(&ModuleName::from_str("TypeError")));
    }

    #[test]
    fn test_try_handler_typed_catch_all() {
        let handler = TryHandler::typed(vec![ModuleName::from_str("Exception")]);
        assert!(handler.catches(&ModuleName::from_str("ValueError")));
        assert!(handler.catches(&ModuleName::from_str("Exception")));

        let handler = TryHandler::typed(vec![ModuleName::from_str("BaseException")]);
        assert!(handler.catches(&ModuleName::from_str("KeyboardInterrupt")));
    }

    #[test]
    fn test_try_handler_typed_tuple() {
        let handler = TryHandler::typed(vec![
            ModuleName::from_str("TypeError"),
            ModuleName::from_str("ValueError"),
        ]);
        assert!(handler.catches(&ModuleName::from_str("ValueError")));
        assert!(handler.catches(&ModuleName::from_str("TypeError")));
        assert!(!handler.catches(&ModuleName::from_str("KeyError")));
    }

    #[test]
    fn test_catches_exception_nested_try() {
        let mut stack = BlockStack::new();
        let outer = Block::TryBody(vec![TryHandler::typed(vec![ModuleName::from_str(
            "OSError",
        )])]);
        let inner = Block::TryBody(vec![TryHandler::typed(vec![ModuleName::from_str(
            "TypeError",
        )])]);
        stack.push(outer);
        stack.push(inner);

        // Inner catches TypeError
        assert!(stack.catches_exception(&ModuleName::from_str("TypeError")));
        // Outer catches OSError
        assert!(stack.catches_exception(&ModuleName::from_str("OSError")));
        // Neither catches ValueError
        assert!(!stack.catches_exception(&ModuleName::from_str("ValueError")));
    }

    #[test]
    fn test_try_handlers_empty_stack() {
        let stack = BlockStack::new();
        let handlers: Vec<&TryHandler> = stack.try_handlers().collect();
        assert!(handlers.is_empty());
    }

    #[test]
    fn test_try_handlers_nested_try() {
        let mut stack = BlockStack::new();
        let outer = Block::TryBody(vec![TryHandler::typed(vec![ModuleName::from_str(
            "OSError",
        )])]);
        let inner = Block::TryBody(vec![
            TryHandler::Bare,
            TryHandler::typed(vec![ModuleName::from_str("TypeError")]),
        ]);
        stack.push(outer);
        stack.push(inner);

        let handlers: Vec<&TryHandler> = stack.try_handlers().collect();
        assert_eq!(handlers.len(), 3);
        assert_eq!(
            handlers[0],
            &TryHandler::Single(ModuleName::from_str("OSError"))
        );
        assert_eq!(handlers[1], &TryHandler::Bare);
        assert_eq!(
            handlers[2],
            &TryHandler::Single(ModuleName::from_str("TypeError"))
        );
    }
}
