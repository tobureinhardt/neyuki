//! The scope stack.

use std::collections::HashMap;

use crate::ast::ty::TypeExpr;
use crate::diagnostics::span::Span;
use crate::sema::symbol::Symbol;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeKind {
    Global,
    Function,
    Block,
    Loop,
}

/// A `type Name = ...` declared in a scope.
pub struct TypeAlias {
    pub ty: TypeExpr,
    pub span: Span,
}

pub struct Scope {
    pub kind: ScopeKind,
    pub symbols: HashMap<String, Symbol>,
    /// Type names live apart from variables: `type Point` and `local Point`
    /// may both exist.
    pub types: HashMap<String, TypeAlias>,
}

impl Scope {
    pub fn new(kind: ScopeKind) -> Self {
        Self {
            kind,
            symbols: HashMap::new(),
            types: HashMap::new(),
        }
    }
}

pub struct ScopeManager {
    pub scopes: Vec<Scope>,
}

impl Default for ScopeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ScopeManager {
    pub fn new() -> Self {
        let mut manager = Self { scopes: Vec::new() };
        manager.enter_scope(ScopeKind::Global);
        manager
    }

    pub fn enter_scope(&mut self, kind: ScopeKind) {
        self.scopes.push(Scope::new(kind));
    }

    pub fn exit_scope(&mut self) -> Vec<Symbol> {
        if self.scopes.len() > 1 {
            let scope = self
                .scopes
                .pop()
                .expect("checked above: more than one scope");
            scope.symbols.into_values().collect()
        } else {
            Vec::new()
        }
    }

    pub fn define(&mut self, symbol: Symbol) -> Result<(), Span> {
        let current = self.scopes.last_mut().expect("scope stack cannot be empty");
        if let Some(existing) = current.symbols.get(&symbol.name)
            && existing.is_const
        {
            return Err(existing.span);
        }
        current.symbols.insert(symbol.name.clone(), symbol);
        Ok(())
    }

    pub fn define_global(&mut self, symbol: Symbol) -> Result<(), Span> {
        let global_scope = self
            .scopes
            .first_mut()
            .expect("scope stack cannot be empty");
        if let Some(existing) = global_scope.symbols.get(&symbol.name)
            && existing.is_const
        {
            return Err(existing.span);
        }
        global_scope.symbols.insert(symbol.name.clone(), symbol);
        Ok(())
    }

    /// Declares a type alias in the current scope. Fails with the span of the
    /// existing declaration when the scope already has one of that name.
    pub fn define_type(&mut self, name: &str, alias: TypeAlias) -> Result<(), Span> {
        let current = self.scopes.last_mut().expect("scope stack cannot be empty");
        if let Some(existing) = current.types.get(name) {
            return Err(existing.span);
        }
        current.types.insert(name.to_string(), alias);
        Ok(())
    }

    /// The innermost alias called `name`.
    pub fn lookup_type(&self, name: &str) -> Option<&TypeAlias> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.types.get(name))
    }

    /// Whether any type alias is in scope, so the common case of a program
    /// without any can skip alias resolution.
    pub fn has_types(&self) -> bool {
        self.scopes.iter().any(|scope| !scope.types.is_empty())
    }

    pub fn lookup(&self, name: &str) -> Option<&Symbol> {
        for scope in self.scopes.iter().rev() {
            if let Some(sym) = scope.symbols.get(name) {
                return Some(sym);
            }
        }
        None
    }

    pub fn lookup_mut(&mut self, name: &str) -> Option<&mut Symbol> {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(sym) = scope.symbols.get_mut(name) {
                return Some(sym);
            }
        }
        None
    }

    pub fn flush_all(&mut self) -> Vec<Symbol> {
        let mut all = Vec::new();
        while let Some(scope) = self.scopes.pop() {
            all.extend(scope.symbols.into_values());
        }
        all
    }

    // Check if symbol exists in an enclosing outer scope
    pub fn find_outer(&self, name: &str) -> Option<&Symbol> {
        if self.scopes.len() <= 1 {
            return None;
        }
        // Exclude the current innermost scope
        for scope in self.scopes[0..self.scopes.len() - 1].iter().rev() {
            if let Some(sym) = scope.symbols.get(name) {
                return Some(sym);
            }
        }
        None
    }

    pub fn mark_used(&mut self, name: &str) -> bool {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(sym) = scope.symbols.get_mut(name) {
                sym.used = true;
                return true;
            }
        }
        false
    }
}
