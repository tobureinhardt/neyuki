//! The source tree the decompiler builds, and how it is printed.

use std::collections::HashSet;

use num_bigint::BigInt;

use super::is_name;
use super::vars::VarId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    IDiv,
    Mod,
    Pow,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    LShl,
    LShr,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Coalesce,
}

impl BinOp {
    fn text(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::IDiv => "//",
            BinOp::Mod => "%",
            BinOp::Pow => "^",
            BinOp::BitAnd => "&",
            BinOp::BitOr => "|",
            BinOp::BitXor => "~",
            BinOp::Shl => "<<",
            BinOp::Shr => ">>",
            BinOp::LShl => "<<<",
            BinOp::LShr => ">>>",
            BinOp::Concat => "..",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "and",
            BinOp::Coalesce => "??",
        }
    }

    /// Binding power, as the parser has it (every level is left
    /// associative).
    fn prec(self) -> u8 {
        match self {
            BinOp::And => 2,
            BinOp::Coalesce => 3,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 4,
            BinOp::BitOr => 5,
            BinOp::BitXor => 6,
            BinOp::BitAnd => 7,
            BinOp::Shl | BinOp::Shr | BinOp::LShl | BinOp::LShr => 8,
            BinOp::Concat => 9,
            BinOp::Add | BinOp::Sub => 10,
            BinOp::Mul | BinOp::Div | BinOp::IDiv | BinOp::Mod => 11,
            BinOp::Pow => 12,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum UnOp {
    Neg,
    Not,
    Len,
    BitNot,
}

/// Unary operators bind tighter than any binary one, `^` included.
const UNARY: u8 = 13;
const ATOM: u8 = 14;

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Expr {
    Nil,
    Bool(bool),
    Int(BigInt),
    Float(f64),
    Str(String),
    Var(VarId),
    /// A global, or an upvalue (named after the enclosing function's
    /// variable).
    Name(String),
    Vararg,
    Index(Box<Expr>, Box<Expr>),
    Call(Box<Expr>, Vec<Expr>),
    Method(Box<Expr>, String, Vec<Expr>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    Un(UnOp, Box<Expr>),
    Table(Vec<Item>),
    Function(Box<Func>),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Item {
    Pos(Expr),
    Keyed(Expr, Expr),
}

impl Expr {
    pub(super) fn bin(op: BinOp, a: Expr, b: Expr) -> Expr {
        Expr::Bin(op, Box::new(a), Box::new(b))
    }

    pub(super) fn index(table: Expr, key: Expr) -> Expr {
        Expr::Index(Box::new(table), Box::new(key))
    }

    /// Whether it can stand for several values at the end of a list.
    pub(super) fn is_multi(&self) -> bool {
        matches!(self, Expr::Call(..) | Expr::Method(..) | Expr::Vararg)
    }

    /// Calls `f` with every variable of this function it reads, including
    /// the ones a nested function captures.
    pub(super) fn each_var(&self, f: &mut impl FnMut(VarId)) {
        match self {
            Expr::Var(v) => f(*v),
            Expr::Index(a, b) | Expr::Bin(_, a, b) => {
                a.each_var(f);
                b.each_var(f);
            }
            Expr::Un(_, a) => a.each_var(f),
            Expr::Call(callee, args) | Expr::Method(callee, _, args) => {
                callee.each_var(f);
                for arg in args {
                    arg.each_var(f);
                }
            }
            Expr::Table(items) => {
                for item in items {
                    match item {
                        Item::Pos(e) => e.each_var(f),
                        Item::Keyed(k, v) => {
                            k.each_var(f);
                            v.each_var(f);
                        }
                    }
                }
            }
            Expr::Function(func) => {
                for &v in &func.captures {
                    f(v);
                }
            }
            _ => {}
        }
    }

    pub(super) fn mentions(&self, var: VarId) -> bool {
        let mut found = false;
        self.each_var(&mut |v| found |= v == var);
        found
    }

    fn prec(&self) -> u8 {
        match self {
            Expr::Bin(op, ..) => op.prec(),
            Expr::Un(..) => UNARY,
            Expr::Int(n) if n.sign() == num_bigint::Sign::Minus => UNARY,
            Expr::Float(x) if x.is_sign_negative() && !x.is_nan() => UNARY,
            _ => ATOM,
        }
    }

    /// Whether it can be called or indexed without parentheses.
    fn is_prefix(&self) -> bool {
        matches!(
            self,
            Expr::Var(_) | Expr::Name(_) | Expr::Index(..) | Expr::Call(..) | Expr::Method(..)
        )
    }
}

/// A decompiled function.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Func {
    pub(super) params: Vec<String>,
    pub(super) vararg: bool,
    pub(super) body: Body,
    /// The names of this function's variables, by id.
    pub(super) names: Vec<String>,
    /// The enclosing function's variables it captures.
    pub(super) captures: Vec<VarId>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Body {
    Code(Vec<Stmt>),
    /// The body could not be recovered; the disassembly stands in for it.
    Lost {
        reason: String,
        listing: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Stmt {
    Local(Vec<VarId>, Vec<Expr>),
    Assign(Vec<Target>, Vec<Expr>),
    Call(Expr),
    /// An expression evaluated only for its side effects.
    Discard(Expr),
    If(Expr, Vec<Stmt>, Vec<Stmt>),
    While {
        cond: Expr,
        body: Vec<Stmt>,
    },
    NumFor {
        var: VarId,
        start: Expr,
        limit: Expr,
        step: Expr,
        body: Vec<Stmt>,
    },
    GenFor {
        vars: Vec<VarId>,
        source: Expr,
        body: Vec<Stmt>,
    },
    Return(Vec<Expr>),
    Break,
    Continue,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Target {
    Var(VarId),
    /// An upvalue.
    Name(String),
    Global(String),
    Index(Expr, Expr),
}

fn indent(depth: usize) -> String {
    "    ".repeat(depth)
}

pub(super) fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\{:03}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Text inside a backtick string.
fn backtick_text(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '`' => out.push_str("\\`"),
            '\\' => out.push_str("\\\\"),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\{:03}", c as u32));
            }
            c => out.push(c),
        }
    }
}

fn float(x: f64) -> String {
    if x.is_nan() {
        "(0 / 0)".to_string()
    } else if x.is_infinite() {
        if x > 0.0 { "math.huge" } else { "-math.huge" }.to_string()
    } else {
        format!("{:?}", x)
    }
}

/// `a.b.c`, which `function a.b.c()` accepts.
fn is_dotted(e: &Expr) -> bool {
    match e {
        Expr::Var(_) | Expr::Name(_) => true,
        Expr::Index(base, key) => {
            matches!(key.as_ref(), Expr::Str(k) if is_name(k)) && is_dotted(base)
        }
        _ => false,
    }
}

/// Prints a chunk. Remembers which globals it already declared, so the
/// first assignment to each one reads `global name = ...`.
#[derive(Default)]
pub(super) struct Printer {
    globals: HashSet<String>,
}

impl Printer {
    pub(super) fn block(
        &mut self,
        out: &mut String,
        stmts: &[Stmt],
        names: &[String],
        depth: usize,
    ) {
        for stmt in stmts {
            self.stmt(out, stmt, names, depth);
        }
    }

    fn line(out: &mut String, depth: usize, text: &str) {
        out.push_str(&indent(depth));
        // A line that starts with `(` would continue the previous one as a
        // call.
        if text.starts_with('(') {
            out.push(';');
        }
        out.push_str(text);
        out.push('\n');
    }

    fn stmt(&mut self, out: &mut String, stmt: &Stmt, names: &[String], depth: usize) {
        match stmt {
            Stmt::Local(vars, vals) => {
                if let ([var], [Expr::Function(f)]) = (vars.as_slice(), vals.as_slice()) {
                    let text = format!("local function {}{}", names[*var], self.func(f, depth));
                    Self::line(out, depth, &text);
                    return;
                }
                let lhs: Vec<&str> = vars.iter().map(|v| names[*v].as_str()).collect();
                let rhs = if vals.is_empty() {
                    vec!["nil".to_string(); vars.len()]
                } else {
                    vals.iter().map(|v| self.expr(v, names, depth)).collect()
                };
                let text = format!("local {} = {}", lhs.join(", "), rhs.join(", "));
                Self::line(out, depth, &text);
            }
            Stmt::Assign(targets, vals) => {
                if let ([Target::Index(base, key)], [Expr::Function(f)]) =
                    (targets.as_slice(), vals.as_slice())
                    && let Expr::Str(k) = key
                    && is_name(k)
                    && is_dotted(base)
                {
                    let base = self.expr(base, names, depth);
                    let text = if f.params.first().is_some_and(|p| p == "self") {
                        let mut method = f.as_ref().clone();
                        method.params.remove(0);
                        format!("function {}:{}{}", base, k, self.func(&method, depth))
                    } else {
                        format!("function {}.{}{}", base, k, self.func(f, depth))
                    };
                    Self::line(out, depth, &text);
                    return;
                }
                let mut declare = false;
                let lhs: Vec<String> = targets
                    .iter()
                    .map(|t| match t {
                        Target::Var(v) => names[*v].clone(),
                        Target::Name(n) => n.clone(),
                        Target::Global(n) => {
                            declare |= self.globals.insert(n.clone());
                            n.clone()
                        }
                        Target::Index(base, key) => self.index(base, key, names, depth),
                    })
                    .collect();
                let rhs: Vec<String> = vals.iter().map(|v| self.expr(v, names, depth)).collect();
                let text = format!(
                    "{}{} = {}",
                    if declare { "global " } else { "" },
                    lhs.join(", "),
                    rhs.join(", ")
                );
                Self::line(out, depth, &text);
            }
            Stmt::Call(e) => {
                let text = self.expr(e, names, depth);
                Self::line(out, depth, &text);
            }
            Stmt::Discard(e) => {
                let text = format!("local _ = {}", self.expr(e, names, depth));
                Self::line(out, depth, &text);
            }
            Stmt::If(cond, then, other) => {
                let text = format!("if {} then", self.expr(cond, names, depth));
                Self::line(out, depth, &text);
                self.block(out, then, names, depth + 1);
                let mut rest = other;
                while let [Stmt::If(cond, then, other)] = rest.as_slice() {
                    let text = format!("elseif {} then", self.expr(cond, names, depth));
                    Self::line(out, depth, &text);
                    self.block(out, then, names, depth + 1);
                    rest = other;
                }
                if !rest.is_empty() {
                    Self::line(out, depth, "else");
                    self.block(out, rest, names, depth + 1);
                }
                Self::line(out, depth, "end");
            }
            Stmt::While { cond, body } => {
                let text = format!("while {} do", self.expr(cond, names, depth));
                Self::line(out, depth, &text);
                self.block(out, body, names, depth + 1);
                Self::line(out, depth, "end");
            }
            Stmt::NumFor {
                var,
                start,
                limit,
                step,
                body,
            } => {
                let mut text = format!(
                    "for {} = {}, {}",
                    names[*var],
                    self.expr(start, names, depth),
                    self.expr(limit, names, depth)
                );
                if *step != Expr::Int(BigInt::from(1)) {
                    text.push_str(", ");
                    text.push_str(&self.expr(step, names, depth));
                }
                text.push_str(" do");
                Self::line(out, depth, &text);
                self.block(out, body, names, depth + 1);
                Self::line(out, depth, "end");
            }
            Stmt::GenFor { vars, source, body } => {
                let vars: Vec<&str> = vars.iter().map(|v| names[*v].as_str()).collect();
                let text = format!(
                    "for {} in {} do",
                    vars.join(", "),
                    self.expr(source, names, depth)
                );
                Self::line(out, depth, &text);
                self.block(out, body, names, depth + 1);
                Self::line(out, depth, "end");
            }
            Stmt::Return(vals) => {
                let text = if vals.is_empty() {
                    "return".to_string()
                } else {
                    let vals: Vec<String> =
                        vals.iter().map(|v| self.expr(v, names, depth)).collect();
                    format!("return {}", vals.join(", "))
                };
                Self::line(out, depth, &text);
            }
            Stmt::Break => Self::line(out, depth, "break"),
            Stmt::Continue => Self::line(out, depth, "continue"),
        }
    }

    /// `(params)`, the body and `end`, for after `function name`.
    fn func(&mut self, f: &Func, depth: usize) -> String {
        let mut params = f.params.clone();
        if f.vararg {
            params.push("...".to_string());
        }
        let mut out = format!("({})\n", params.join(", "));
        match &f.body {
            Body::Code(stmts) => self.block(&mut out, stmts, &f.names, depth + 1),
            Body::Lost { reason, listing } => {
                let pad = indent(depth + 1);
                out.push_str(&format!(
                    "{}-- neyuki decompile: {}. The function's bytecode:\n",
                    pad, reason
                ));
                for line in listing.lines() {
                    out.push_str(&format!("{}--   {}\n", pad, line));
                }
                out.push_str(&format!(
                    "{}error(\"neyuki decompile: this function could not be recovered\")\n",
                    pad
                ));
            }
        }
        out.push_str(&indent(depth));
        out.push_str("end");
        out
    }

    fn index(&mut self, base: &Expr, key: &Expr, names: &[String], depth: usize) -> String {
        let base = self.prefix(base, names, depth);
        match key {
            Expr::Str(k) if is_name(k) => format!("{}.{}", base, k),
            key => format!("{}[{}]", base, self.expr(key, names, depth)),
        }
    }

    fn prefix(&mut self, e: &Expr, names: &[String], depth: usize) -> String {
        let text = self.expr(e, names, depth);
        if e.is_prefix() {
            text
        } else {
            format!("({})", text)
        }
    }

    /// `e` for a place that needs at least binding power `min`.
    fn operand(&mut self, e: &Expr, min: u8, names: &[String], depth: usize) -> String {
        let text = self.expr(e, names, depth);
        if e.prec() < min {
            format!("({})", text)
        } else {
            text
        }
    }

    fn list(&mut self, items: &[Expr], names: &[String], depth: usize) -> String {
        let items: Vec<String> = items.iter().map(|e| self.expr(e, names, depth)).collect();
        items.join(", ")
    }

    /// A chain of `..` that starts with `""` is how string interpolation
    /// compiles; it prints as a backtick string when every piece fits.
    fn interpolation(&mut self, e: &Expr, names: &[String], depth: usize) -> Option<String> {
        let mut pieces = Vec::new();
        let mut cur = e;
        while let Expr::Bin(BinOp::Concat, left, right) = cur {
            pieces.push(right.as_ref());
            cur = left;
        }
        if *cur != Expr::Str(String::new()) || pieces.iter().all(|p| matches!(p, Expr::Str(_))) {
            return None;
        }
        let mut out = String::from("`");
        for piece in pieces.into_iter().rev() {
            match piece {
                Expr::Str(s) => backtick_text(s, &mut out),
                other => {
                    let text = self.expr(other, names, depth);
                    if text.contains(['`', '{', '}', '\n']) {
                        return None;
                    }
                    out.push('{');
                    out.push_str(&text);
                    out.push('}');
                }
            }
        }
        out.push('`');
        Some(out)
    }

    pub(super) fn expr(&mut self, e: &Expr, names: &[String], depth: usize) -> String {
        match e {
            Expr::Nil => "nil".to_string(),
            Expr::Bool(b) => b.to_string(),
            Expr::Int(n) => n.to_string(),
            Expr::Float(x) => float(*x),
            Expr::Str(s) => quote(s),
            Expr::Var(v) => names[*v].clone(),
            Expr::Name(n) => n.clone(),
            Expr::Vararg => "...".to_string(),
            Expr::Index(base, key) => self.index(base, key, names, depth),
            Expr::Call(callee, args) => {
                let callee = self.prefix(callee, names, depth);
                format!("{}({})", callee, self.list(args, names, depth))
            }
            Expr::Method(obj, name, args) => {
                let obj = self.prefix(obj, names, depth);
                format!("{}:{}({})", obj, name, self.list(args, names, depth))
            }
            Expr::Bin(op, left, right) => {
                if *op == BinOp::Concat
                    && let Some(text) = self.interpolation(e, names, depth)
                {
                    return text;
                }
                let p = op.prec();
                let left = self.operand(left, p, names, depth);
                let right = self.operand(right, p + 1, names, depth);
                format!("{} {} {}", left, op.text(), right)
            }
            Expr::Un(op, inner) => {
                let text = self.operand(inner, UNARY, names, depth);
                match op {
                    UnOp::Not => format!("not {}", text),
                    UnOp::Len => format!("#{}", text),
                    // `- -x` keeps the two signs from reading as a comment.
                    UnOp::Neg if text.starts_with('-') => format!("-({})", text),
                    UnOp::Neg => format!("-{}", text),
                    UnOp::BitNot if text.starts_with('~') => format!("~({})", text),
                    UnOp::BitNot => format!("~{}", text),
                }
            }
            Expr::Table(items) => {
                if items.is_empty() {
                    return "{}".to_string();
                }
                let items: Vec<String> = items
                    .iter()
                    .map(|item| match item {
                        Item::Pos(v) => self.expr(v, names, depth),
                        Item::Keyed(Expr::Str(k), v) if is_name(k) => {
                            format!("{} = {}", k, self.expr(v, names, depth))
                        }
                        Item::Keyed(k, v) => format!(
                            "[{}] = {}",
                            self.expr(k, names, depth),
                            self.expr(v, names, depth)
                        ),
                    })
                    .collect();
                format!("{{ {} }}", items.join(", "))
            }
            Expr::Function(f) => format!("function{}", self.func(f, depth)),
        }
    }
}
