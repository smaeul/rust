//! A "Parser" structure for token trees. We use this when parsing a declarative
//! macro definition into a list of patterns and templates.

use crate::{to_parser_tokens::to_parser_tokens, ExpandError, ExpandResult, ParserEntryPoint};

use syntax::SyntaxKind;
use tt::buffer::TokenBuffer;

macro_rules! err {
    () => {
        ExpandError::BindingError(format!(""))
    };
    ($($tt:tt)*) => {
        ExpandError::BindingError(format!($($tt)*))
    };
}

#[derive(Debug, Clone)]
pub(crate) struct TtIter<'a> {
    pub(crate) inner: std::slice::Iter<'a, tt::TokenTree>,
}

impl<'a> TtIter<'a> {
    pub(crate) fn new(subtree: &'a tt::Subtree) -> TtIter<'a> {
        TtIter { inner: subtree.token_trees.iter() }
    }

    pub(crate) fn expect_char(&mut self, char: char) -> Result<(), ()> {
        match self.next() {
            Some(tt::TokenTree::Leaf(tt::Leaf::Punct(tt::Punct { char: c, .. }))) if *c == char => {
                Ok(())
            }
            _ => Err(()),
        }
    }

    pub(crate) fn expect_any_char(&mut self, chars: &[char]) -> Result<(), ()> {
        match self.next() {
            Some(tt::TokenTree::Leaf(tt::Leaf::Punct(tt::Punct { char: c, .. })))
                if chars.contains(c) =>
            {
                Ok(())
            }
            _ => Err(()),
        }
    }

    pub(crate) fn expect_subtree(&mut self) -> Result<&'a tt::Subtree, ()> {
        match self.next() {
            Some(tt::TokenTree::Subtree(it)) => Ok(it),
            _ => Err(()),
        }
    }

    pub(crate) fn expect_leaf(&mut self) -> Result<&'a tt::Leaf, ()> {
        match self.next() {
            Some(tt::TokenTree::Leaf(it)) => Ok(it),
            _ => Err(()),
        }
    }

    pub(crate) fn expect_ident(&mut self) -> Result<&'a tt::Ident, ()> {
        match self.expect_leaf()? {
            tt::Leaf::Ident(it) if it.text != "_" => Ok(it),
            _ => Err(()),
        }
    }

    pub(crate) fn expect_ident_or_underscore(&mut self) -> Result<&'a tt::Ident, ()> {
        match self.expect_leaf()? {
            tt::Leaf::Ident(it) => Ok(it),
            _ => Err(()),
        }
    }

    pub(crate) fn expect_literal(&mut self) -> Result<&'a tt::Leaf, ()> {
        let it = self.expect_leaf()?;
        match it {
            tt::Leaf::Literal(_) => Ok(it),
            tt::Leaf::Ident(ident) if ident.text == "true" || ident.text == "false" => Ok(it),
            _ => Err(()),
        }
    }

    pub(crate) fn expect_punct(&mut self) -> Result<&'a tt::Punct, ()> {
        match self.expect_leaf()? {
            tt::Leaf::Punct(it) => Ok(it),
            _ => Err(()),
        }
    }

    pub(crate) fn expect_fragment(
        &mut self,
        entry_point: ParserEntryPoint,
    ) -> ExpandResult<Option<tt::TokenTree>> {
        let buffer = TokenBuffer::from_tokens(self.inner.as_slice());
        let parser_tokens = to_parser_tokens(&buffer);
        let tree_traversal = parser::parse(&parser_tokens, entry_point);

        let mut cursor = buffer.begin();
        let mut error = false;
        for step in tree_traversal.iter() {
            match step {
                parser::TraversalStep::Token { kind, mut n_raw_tokens } => {
                    if kind == SyntaxKind::LIFETIME_IDENT {
                        n_raw_tokens = 2;
                    }
                    for _ in 0..n_raw_tokens {
                        cursor = cursor.bump_subtree();
                    }
                }
                parser::TraversalStep::EnterNode { .. } | parser::TraversalStep::LeaveNode => (),
                parser::TraversalStep::Error { .. } => error = true,
            }
        }

        let mut err = if !cursor.is_root() || error {
            Some(err!("expected {:?}", entry_point))
        } else {
            None
        };

        let mut curr = buffer.begin();
        let mut res = vec![];

        if cursor.is_root() {
            while curr != cursor {
                if let Some(token) = curr.token_tree() {
                    res.push(token);
                }
                curr = curr.bump();
            }
        }
        self.inner = self.inner.as_slice()[res.len()..].iter();
        if res.is_empty() && err.is_none() {
            err = Some(err!("no tokens consumed"));
        }
        let res = match res.len() {
            1 => Some(res[0].cloned()),
            0 => None,
            _ => Some(tt::TokenTree::Subtree(tt::Subtree {
                delimiter: None,
                token_trees: res.into_iter().map(|it| it.cloned()).collect(),
            })),
        };
        ExpandResult { value: res, err }
    }

    pub(crate) fn peek_n(&self, n: usize) -> Option<&tt::TokenTree> {
        self.inner.as_slice().get(n)
    }
}

impl<'a> Iterator for TtIter<'a> {
    type Item = &'a tt::TokenTree;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<'a> std::iter::ExactSizeIterator for TtIter<'a> {}
