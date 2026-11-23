//! Statement and declaration attributes (`@attr`, `@[attr]`, `#[attr]`, `[attr]`).

use std::collections::HashMap;
use std::fmt;

use crate::ast::literal::Literal;
use crate::ast::node_id::NodeId;
use crate::ast::span::Span;

/// An argument inside an attribute list, e.g. `@allow(unused)`,
/// `@deprecated("use foo")`, or `@inline(always = true)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttributeArg {
    Ident(String),
    Literal(Literal),
    Named { key: String, value: Literal },
}

impl fmt::Display for AttributeArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ident(ident) => write!(f, "{}", ident),
            Self::Literal(lit) => write!(f, "{}", lit),
            Self::Named { key, value } => write!(f, "{} = {}", key, value),
        }
    }
}

/// A parsed attribute attached to a statement or declaration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attribute {
    pub name: String,
    pub args: Vec<AttributeArg>,
    pub span: Span,
}

impl Attribute {
    pub fn new(name: impl Into<String>, args: Vec<AttributeArg>, span: Span) -> Self {
        Self {
            name: name.into(),
            args,
            span,
        }
    }

    /// Returns the first string literal argument if present, e.g. `@deprecated("reason")`.
    pub fn first_string_arg(&self) -> Option<&str> {
        self.args.iter().find_map(|arg| match arg {
            AttributeArg::Literal(Literal::String(s)) => Some(s.as_str()),
            _ => None,
        })
    }

    /// Returns true if an identifier argument matching `ident` is present.
    pub fn has_ident_arg(&self, ident: &str) -> bool {
        self.args
            .iter()
            .any(|arg| matches!(arg, AttributeArg::Ident(name) if name == ident))
    }
}

impl fmt::Display for Attribute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "@{}", self.name)?;
        if !self.args.is_empty() {
            write!(f, "(")?;
            for (idx, arg) in self.args.iter().enumerate() {
                if idx > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{}", arg)?;
            }
            write!(f, ")")?;
        }
        Ok(())
    }
}

/// Side table mapping statement/declaration `NodeId`s to their attached attributes,
/// mirroring `SpanPool`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AttributePool {
    map: HashMap<NodeId, Vec<Attribute>>,
}

impl AttributePool {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    pub fn insert(&mut self, id: NodeId, attrs: Vec<Attribute>) {
        if !attrs.is_empty() {
            self.map.insert(id, attrs);
        }
    }

    pub fn append(&mut self, id: NodeId, mut attrs: Vec<Attribute>) {
        if attrs.is_empty() {
            return;
        }
        self.map.entry(id).or_default().append(&mut attrs);
    }

    pub fn get(&self, id: NodeId) -> &[Attribute] {
        self.map.get(&id).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn has(&self, id: NodeId, name: &str) -> bool {
        self.get(id).iter().any(|attr| attr.name == name)
    }

    pub fn find(&self, id: NodeId, name: &str) -> Option<&Attribute> {
        self.get(id).iter().find(|attr| attr.name == name)
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&NodeId, &Vec<Attribute>)> {
        self.map.iter()
    }
}
