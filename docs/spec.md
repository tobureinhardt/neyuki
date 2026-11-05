# Neyuki Language Specification (Version 1.0 Freeze Draft)

This document defines the formal grammar, lexical structure, type system, and evaluation semantics of the Neyuki programming language.

## 1. Lexical Grammar

### 1.1 Whitespace and Comments
- Whitespace consists of spaces (`0x20`), tabs (`0x09`), carriage returns (`0x0D`), and newlines (`0x0A`). Whitespace separates tokens and is otherwise ignored except within string literals.
- Single-line comments start with `--` and continue to the end of the line.
- Multi-line comments start with `--[[` and terminate at the matching `]]`.

### 1.2 Identifiers and Keywords
- Identifiers match the regular expression `[a-zA-Z_][a-zA-Z0-9_]*`.
- Reserved keywords:
  ```text
  and       break     case      const     continue  default
  defer     do        else      elseif    end       enum
  false     for       function  global    if        in
  local     match     nil       not       or        repeat
  return    then      true      type      until     while
  ```

### 1.3 Literals
- **Nil**: `nil`
- **Boolean**: `true`, `false`
- **Integer**:
  - Decimal: `0`, `[1-9][0-9_]*`
  - Hexadecimal: `0x[0-9a-fA-F_]+`
  - Binary: `0b[01_]+`
  - Integers have arbitrary precision. Small values fit within signed 64-bit machine words; values outside that range automatically promote to arbitrary-precision BigInt.
- **Float**: Decimal floating-point numbers conforming to standard IEEE 754 64-bit float format (`[0-9]+\.[0-9]+([eE][+-]?[0-9]+)?`).
- **String**:
  - Quoted: `"..."` or `'...'` with standard escape sequences (`\n`, `\r`, `\t`, `\\`, `\"`, `\'`, `\xNN`, `\u{NNNN}`).
  - Multi-line long bracket: `[[...]]`.
  - Interpolated string: `` `text {expr} more text` ``.

---

## 2. Syntax Grammar (EBNF)

```ebnf
Program         ::= StatementList

StatementList   ::= { Statement [ ";" ] }

Statement       ::= BindingStmt
                  | AssignStmt
                  | IncrementStmt
                  | FunctionStmt
                  | IfStmt
                  | ForStmt
                  | WhileStmt
                  | RepeatStmt
                  | MatchStmt
                  | EnumStmt
                  | ReturnStmt
                  | BreakStmt
                  | ContinueStmt
                  | GotoStmt
                  | LabelStmt
                  | TypeAliasStmt
                  | ExprStmt

TypeAliasStmt   ::= [ "export" ] "type" IDENTIFIER "=" TypeExpr
                    (* contextual: only when "type" (or "export" "type") is followed by
                       IDENTIFIER and "=". "export" is only allowed at the top level.
                       "type" IDENTIFIER "<" ... is rejected: generics are not supported. *)

BindingStmt     ::= ( "local" | "const" | "global" ) BindingDecl { "," BindingDecl }
BindingDecl     ::= IDENTIFIER [ ":" TypeExpr ] [ "=" Expr ]

AssignStmt      ::= AssignTargetList "=" ExprList
                  | AssignTarget CompoundOp Expr
AssignTargetList::= AssignTarget { "," AssignTarget }
AssignTarget    ::= IDENTIFIER
                  | PrimaryExpr "." IDENTIFIER
                  | PrimaryExpr "[" Expr "]"
CompoundOp      ::= "+=" | "-=" | "*=" | "/=" | "//=" | "%=" | "^="
                  | "..=" | "<<=" | ">>=" | "&=" | "|=" | "??="

IncrementStmt   ::= AssignTarget ( "++" | "--" )

FunctionStmt    ::= [ "const" ] "function" [ IDENTIFIER [ "." IDENTIFIER | ":" IDENTIFIER ] ]
                    "(" [ ParamList ] ")" [ ":" TypeExpr ]
                    StatementList
                    "end"

ParamList       ::= Param { "," Param } [ "," VarargParam ] | VarargParam
Param           ::= IDENTIFIER [ ":" TypeExpr ]
VarargParam     ::= "..." [ ":" TypeExpr ]

IfStmt          ::= "if" Expr "then"
                    StatementList
                    { "elseif" Expr "then" StatementList }
                    [ "else" StatementList ]
                    "end"

ForStmt         ::= "for" IDENTIFIER "=" Expr "," Expr [ "," Expr ] "do"
                    StatementList
                    "end"
                  | "for" NameList "in" ExprList "do"
                    StatementList
                    "end"

NameList        ::= IDENTIFIER { "," IDENTIFIER }

WhileStmt       ::= "while" Expr "do"
                    StatementList
                    "end"

RepeatStmt      ::= "repeat"
                    StatementList
                    "until" Expr

MatchStmt       ::= "match" Expr
                    { MatchCase }
                    [ MatchDefault ]
                    "end"
MatchCase       ::= "case" Pattern [ "if" Expr ] ":" StatementList
MatchDefault    ::= "default" ":" StatementList
                  | "case" "_" ":" StatementList

Pattern         ::= LiteralPattern
                  | VariablePattern
                  | VariantPattern
                  | WildcardPattern
LiteralPattern  ::= Literal
VariablePattern ::= IDENTIFIER
VariantPattern  ::= IDENTIFIER "." IDENTIFIER [ "(" [ PatternList ] ")" ]
WildcardPattern ::= "_"
PatternList     ::= Pattern { "," Pattern }

EnumStmt        ::= "enum" IDENTIFIER
                    EnumVariant { "," EnumVariant } [ "," ]
                    "end"
EnumVariant     ::= IDENTIFIER [ "(" EnumFieldList ")" ]
EnumFieldList   ::= IDENTIFIER { "," IDENTIFIER }

ReturnStmt      ::= "return" [ ExprList ]
BreakStmt       ::= "break"
ContinueStmt    ::= "continue"
GotoStmt        ::= "goto" IDENTIFIER
LabelStmt       ::= "::" IDENTIFIER "::"
ExprStmt        ::= Expr

ExprList        ::= Expr { "," Expr }
```

---

## 3. Expression Grammar and Operator Precedence

Precedence hierarchy from lowest to highest:

| Level | Operators | Associativity | Description |
|:---|:---|:---|:---|
| 1 | `or` | Left | Logical OR |
| 2 | `and` | Left | Logical AND |
| 3 | `??` | Left | Nil-coalescing |
| 4 | `==`, `!=`, `<`, `<=`, `>`, `>=` | Left | Relational comparisons |
| 5 | `\|` | Left | Bitwise OR |
| 6 | `~` | Left | Bitwise XOR |
| 7 | `&` | Left | Bitwise AND |
| 8 | `<<`, `>>`, `<<<`, `>>>` | Left | Bitwise & logical shifts |
| 9 | `..` | Right | String concatenation |
| 10 | `+`, `-` | Left | Addition, subtraction |
| 11 | `*`, `/`, `//`, `%` | Left | Multiplication, division, int division, modulo |
| 12 | `not`, `#`, `-`, `~` | Right (prefix) | Logical NOT, length, unary negation, bitwise NOT |
| 13 | `^` | Right | Exponentiation |
| 14 | Postfix calls, indexing | Left | `()`, `[]`, `.`, `:` |

---

## 4. Formal Type System Specification

Neyuki features a gradual, sound static type system with optional runtime enforcement.

### 4.1 Type Expressions Syntax
```ebnf
TypeExpr        ::= UnionType
UnionType       ::= PostfixType { "|" PostfixType }
PostfixType     ::= PrimaryType [ "?" ]
PrimaryType     ::= PrimitiveType
                  | ArrayType
                  | RecordType
                  | TableMapType
                  | FunctionType
                  | "(" TypeExpr ")"

PrimitiveType   ::= "any" | "nil" | "void" | "never"
                  | "bool" | "boolean"
                  | "int" | "integer"
                  | "float"
                  | "number"
                  | "string"
                  | "table"
                  | "buffer"
                  | "thread"
                  | IDENTIFIER
                  | IDENTIFIER "." IDENTIFIER   (* qualified: not checked *)

ArrayType       ::= "[" TypeExpr "]"
                  | "{" TypeExpr "}"

RecordType      ::= "{" [ RecordFieldList ] "}"
RecordFieldList ::= RecordField { "," RecordField } [ "," ]
RecordField     ::= IDENTIFIER ":" TypeExpr

TableMapType    ::= "{" "[" TypeExpr "]" ":" TypeExpr "}"

FunctionType    ::= "(" [ ParamTypeList ] ")" "->" TypeExpr
                  | TypeExpr "->" TypeExpr
ParamTypeList   ::= ParamType { "," ParamType }
ParamType       ::= [ IDENTIFIER ":" ] TypeExpr
```

A `TypeAliasStmt` names a type for its scope (blocks scope aliases like locals; forward and recursive references are allowed, a recursive reference is treated as `any`). It is erased before code generation. An `IDENTIFIER` that no alias declares, and any qualified name, is an unchecked named type.

### 4.2 Subtyping and Assignability Rules
1. **Reflexivity**: For any type `T`, `T <: T`.
2. **Top and Bottom**:
   - For all types `T`, `T <: any`.
   - For all types `T`, `never <: T`.
3. **Numeric Hierarchy**:
   - `int <: number`.
   - `float <: number`.
4. **Union Types**:
   - `T <: (U1 | ... | Un)` if exists `k` such that `T <: Uk`.
   - `(T1 | ... | Tn) <: U` if for all `k`, `Tk <: U`.
5. **Optional Types**:
   - `T?` is shorthand for `T | nil`.
6. **Structural Record Types**:
   - Record `R1 = { f1: T1, ..., fn: Tn, ... }` is a subtype of `R2 = { f1: U1, ..., fn: Un }` if for each field `fi` in `R2`, `R1` contains `fi` and `R1.fi <: R2.fi` (depth and width subtyping).
7. **Array and Table Types**:
   - `[T] <: [U]` if `T <: U` (covariant read).
8. **Function Types**:
   - `(A1, ..., An) -> R1 <: (B1, ..., Bn) -> R2` if and only if:
     - For all `i`, `Bi <: Ai` (contravariant parameters).
     - `R1 <: R2` (covariant return type).

---

## 5. Runtime Semantics and Memory Model

### 5.1 Call Stack
- Default recursion limit: 16,384 call frames. Configurable via `NEYUKI_MAX_CALL_DEPTH` environment variable.
- Proper tail calls: Function calls executed in tail position recycle the current frame without allocating a new stack frame.

### 5.2 Garbage Collection
- Managed heap with mark-and-sweep and incremental cycle resolution.
- Tables, closures, buffers, and coroutines participate in automatic reclamation.
- Coroutines dropped without active handles are collected deterministically.

### 5.3 Error Handling
- Exceptions are raised via `error(msg)` or runtime violations (division by zero, assertion failures).
- Protected execution using `try(f, ...)` or `pcall(f, ...)`.
- Uncaught exceptions produce structured diagnostics conforming to compiler diagnostics standard `runtime error[E0200]` with file, line, column, source gutter context, and stack backtrace.
