//! Error and warning codes.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    // Semantic errors (E0001 - E0049)
    E0001, // Undeclared variable used
    E0002, // Reassignment to const variable
    E0003, // Type mismatch
    E0004, // Argument count / arity mismatch
    E0005, // Duplicate declaration in the same scope

    // Syntax & Parsing errors (E0050 - E0099)
    E0050, // Generic syntax error
    E0051, // Unexpected token
    E0052, // Unclosed string or bracket

    // Compilation & Bytecode errors (E0100 - E0149)
    E0100, // Register limit exceeded
    E0101, // Constant pool limit exceeded
    E0102, // Jump offset overflow
    E0103, // Bytecode verification failed

    // Runtime errors (E0200 - E0299)
    E0200, // Runtime exception
    E0201, // Assertion failed

    // Warnings (W0001 - W0099)
    W0001, // Unused variable
    W0002, // Unused parameter
    W0003, // Variable shadowing
    W0004, // Unreachable code
    W0005, // Redundant expression or statement
    W0006, // Byte escape above ASCII in a string literal
}

impl ErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::E0001 => "E0001",
            ErrorCode::E0002 => "E0002",
            ErrorCode::E0003 => "E0003",
            ErrorCode::E0004 => "E0004",
            ErrorCode::E0005 => "E0005",
            ErrorCode::E0050 => "E0050",
            ErrorCode::E0051 => "E0051",
            ErrorCode::E0052 => "E0052",
            ErrorCode::E0100 => "E0100",
            ErrorCode::E0101 => "E0101",
            ErrorCode::E0102 => "E0102",
            ErrorCode::E0103 => "E0103",
            ErrorCode::E0200 => "E0200",
            ErrorCode::E0201 => "E0201",
            ErrorCode::W0001 => "W0001",
            ErrorCode::W0002 => "W0002",
            ErrorCode::W0003 => "W0003",
            ErrorCode::W0004 => "W0004",
            ErrorCode::W0005 => "W0005",
            ErrorCode::W0006 => "W0006",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            ErrorCode::E0001 => "variable used before declaration",
            ErrorCode::E0002 => "cannot reassign to const variable",
            ErrorCode::E0003 => "type mismatch",
            ErrorCode::E0004 => "wrong number of arguments",
            ErrorCode::E0005 => "duplicate declaration in scope",
            ErrorCode::E0050 => "syntax error",
            ErrorCode::E0051 => "unexpected token",
            ErrorCode::E0052 => "unclosed token",
            ErrorCode::E0100 => "register limit exceeded",
            ErrorCode::E0101 => "constant pool limit exceeded",
            ErrorCode::E0102 => "jump offset overflow",
            ErrorCode::E0103 => "bytecode verification failed",
            ErrorCode::E0200 => "runtime error",
            ErrorCode::E0201 => "assertion failed",
            ErrorCode::W0001 => "unused variable",
            ErrorCode::W0002 => "unused parameter",
            ErrorCode::W0003 => "variable shadows outer declaration",
            ErrorCode::W0004 => "unreachable code",
            ErrorCode::W0005 => "redundant operation",
            ErrorCode::W0006 => "byte escape above ASCII is a code point",
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}
