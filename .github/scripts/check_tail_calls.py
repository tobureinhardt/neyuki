#!/usr/bin/env python3
"""Fail if any threaded-interpreter handler dispatches with a real call.

Every handler in src/vm/threaded.rs ends by calling the next handler. Rust
does not guarantee that call becomes a jump; when it does not, the native
stack grows by one frame per executed instruction and long-running scripts
overflow it. This disassembles an unstripped x86_64 release build and
reports, for each function that takes the dispatch table:

  - an indirect `call *(%reg,%reg,8)`, a dispatch through the table that is
    not a tail jump, and
  - a direct `call` to another function that (transitively) dispatches.

Usage: check_tail_calls.py <unstripped neyuki binary> <src/vm/threaded.rs>
"""

import re
import subprocess
import sys

PREFIX = "neyuki::vm::threaded::"
TABLE_CALL = re.compile(r"\bcall\s+\*\(%r\w+,%r\w+,8\)")
TABLE_ANY = re.compile(r"\b(call|jmp)\s+\*\(%r\w+,%r\w+,8\)")
DIRECT = re.compile(r"\b(call|jmp)\s+[0-9a-f]+ <(" + re.escape(PREFIX) + r"[^>+]*)>")


def main():
    binary, source = sys.argv[1], sys.argv[2]
    asm = subprocess.run(
        ["objdump", "-d", "-C", "--no-show-raw-insn", binary],
        capture_output=True,
        text=True,
        check=True,
    ).stdout

    functions = {}
    current = None
    for line in asm.splitlines():
        header = re.match(r"^[0-9a-f]+ <(.*)>:$", line)
        if header:
            name = header.group(1)
            current = name if name.startswith(PREFIX) else None
            if current:
                functions[current] = []
        elif current:
            functions[current].append(line)

    # Only functions that receive the dispatch table can dispatch; others
    # may call through jump tables of plain function pointers.
    text = open(source).read()
    takes_table = {m.group(1) for m in re.finditer(r"\bfn (\w+)\(([^)]*)\)", text) if "DispatchTbl" in m.group(2)}
    functions = {
        f: lines
        for f, lines in functions.items()
        if f[len(PREFIX):] in takes_table or f[len(PREFIX):].startswith("op_")
    }

    dispatching = {f for f, lines in functions.items() if any(TABLE_ANY.search(l) for l in lines)}
    changed = True
    while changed:
        changed = False
        for f, lines in functions.items():
            if f in dispatching:
                continue
            for l in lines:
                m = DIRECT.search(l)
                if m and m.group(2) in dispatching:
                    dispatching.add(f)
                    changed = True
                    break

    problems = []
    for f, lines in functions.items():
        for l in lines:
            if TABLE_CALL.search(l):
                problems.append(f"{f}: dispatch is a call, not a tail jump: {l.strip()}")
            m = DIRECT.search(l)
            if m and m.group(1) == "call" and m.group(2) in dispatching:
                problems.append(f"{f}: calls dispatching {m.group(2)} without a tail call")

    if len(dispatching) < 50:
        print(f"only {len(dispatching)} dispatching handlers found; is the binary stripped?")
        return 1
    for p in problems:
        print(p)
    print(f"{len(dispatching)} dispatching handlers, {len(problems)} non-tail dispatches")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
