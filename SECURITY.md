# Security policy

## Pre-release reports
**To keep development as fast as possible, you can report vulnerabilities from betas & pre-releases through Discord.**
To do so, join our Discord [Server](https://discord.gg/mvCB3KnAT4) and Direct Message one of the Maintainers.
(Pre-1.0.0 is considered pre-release; this snippet will be deleted once we have a stable build)

## Reporting a vulnerability

**Please do not report security vulnerabilities through public GitHub issues, pull requests, or the Discord server.**

Report them privately through GitHub's private vulnerability reporting:

https://github.com/neyuki-lang/neyuki/security/advisories/new

### What to include

Make sure it contains the following:

**Summary**
- What's broken and why it matters. "Integer overflow in bigint conversion leads to memory corruption" is useful; "crash" is not.

**Reproduction steps**
- A minimal `.nyk` script (or Rust snippet) that triggers the issue, with everything not needed stripped out.
- The exact commands you ran (e.g. `cargo run -- run poc.nyk`).
- Expected vs. actual behaviour.

**Severity and impact**
- What can an attacker actually do? (crash, memory corruption, data disclosure, code execution, ...)
- Who is affected: does it require untrusted input, a specific build flag, a specific library, a specific target platform?

**Environment**
- Neyuki version or commit hash.
- OS and architecture.
- Rust toolchain version and any build flags or relevant configuration.

**Root cause** (if known)
- Where in the code the bug lives and why it happens, plus a suggested fix if you have one.

**Proof of concept**
- Crash log, panic message and backtrace (`RUST_BACKTRACE=1`), or a screenshot/GIF for anything visual.

We will acknowledge the report as soon as we can, keep you informed while we work on a fix, and credit you in the advisory unless you prefer to stay anonymous. Please give us a reasonable amount of time to release a fix before disclosing publicly.

## What counts as a vulnerability

Neyuki is pre-1.0 and things are still moving quickly, but the following are always treated as security issues:

- memory unsafety (crashes, use-after-free, out-of-bounds reads/writes) in the VM, garbage collector or any native library reachable from a `.nyk` script
- bugs in `@neyuki/crypto` that weaken hashing, password hashing, encryption, signatures or random number generation
- bugs in `@nyk/sql` (an official package) that allow parameterized queries to be turned into SQL injection
- bugs in `@neyuki/http` that allow request smuggling, header injection, TLS verification bypass or path traversal in the server
- any way for a script to gain capabilities that the runtime is documented as not granting it

### Out of scope

- **Running untrusted scripts.** Neyuki is not a sandbox. Scripts have access to the filesystem, network and OS through the bundled libraries, so a malicious `.nyk` file can do anything the user running it can do. Do not run scripts you do not trust.
- Denial of service through scripts that intentionally allocate unbounded memory or loop forever.
- Vulnerabilities in third-party dependencies that are already reported upstream. Please report those to the crate's maintainers; a PR bumping the dependency is welcome.

## Supported versions

Only the latest release and the `main` branch receive security fixes.

## Questions

If you are unsure whether something is a security issue, ask privately through the advisory link above rather than in public. For everything else, the Discord server is the place: https://discord.gg/mvCB3KnAT4
