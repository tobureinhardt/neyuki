# Neyuki Technical Documentation

This directory contains the formal specifications, guides, and technical reference manuals for the Neyuki programming language, compiler toolchain, and runtime engine.

## Documentation Index

- **[The Language](language.md)**: A tour of the syntax and semantics, from variables to objects and modules.
- **[Library Reference](reference/README.md)**: Every built-in function and bundled module, one section per function.
- **[Language Specification](spec.md)**: Formal EBNF grammar, lexical rules, expression precedence, and static type system.
- **[Bytecode Specification](bytecode_v4.md)**: Binary format, header layout, constant pool encoding, prototype structure, and complete opcode instruction reference.
- **[Module System Reference](modules.md)**: Module isolation, export conventions, `require` resolution algorithm, caching semantics, and security rules.
- **[Versioning & Compatibility Policy](compatibility.md)**: Semantic Versioning enforcement, stability tiers, bytecode longevity, and deprecation lifecycle.
- **[Tools and Runtime](runtime.md)**: The `neyuki` command, virtual machine architecture, linting, bytecode, packages, memory, and limits.

Every example is a `.nyk` file you can run with `neyuki file.nyk`.

## Verification and Testing

All examples and specifications are continuously validated against the test suite:

```bash
cargo test -- --test-threads=1
```
