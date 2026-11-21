# Neyuki Bytecode Specification (Format 7)

This document specifies the binary format, encoding scheme, constant pool structure, prototype hierarchy, and instruction set for Neyuki Bytecode Format 8 (`BYTECODE_VERSION = 8`, with loader backward compatibility for formats 6 and 7 via `MIN_SUPPORTED_BYTECODE_VERSION = 6`).

## 1. File Structure and Header

A compiled `.nykb` binary file begins with a 12-byte header:

```text
+-----------------------+---------+------------------------+-------------------+
| Magic Bytes (7 bytes) | Ver (1) | CRC-32 Checksum (4 LE) | Serialized Protos |
| 'n' 'e' 'y' 'u' 'k'   |  0x07   | Little-Endian u32      | (Variable length) |
| 'i' '!'               |         |                        |                   |
+-----------------------+---------+------------------------+-------------------+
0                       7         8                        12
```

- **Magic**: `b"neyuki!"` (ASCII `0x6E 0x65 0x79 0x75 0x6B 0x69 0x21`).
- **Version**: Single byte `0x07` (`0x06` is also accepted by the deserializer for format 6 `.nykb` files; any version outside `6..=7` is rejected).
- **Checksum**: 32-bit unsigned integer (Little-Endian) computed using standard IEEE 802.3 CRC-32 (`0xEDB88320`) polynomial over all serialized proto payload bytes following the header.

---

## 2. Serialization Format

All multi-byte integers in the payload use standard Little-Endian encoding.

### 2.1 Primitive Types
- `u8`: 1 byte.
- `u16`: 2 bytes (LE).
- `u32`: 4 bytes (LE).
- `i16`: 2 bytes signed (LE).
- `i32`: 4 bytes signed (LE).
- `i64`: 8 bytes signed (LE).
- `f64`: 8 bytes IEEE 754 float (LE).
- `String`: `u32` byte length followed by UTF-8 bytes.

### 2.2 Constant Pool (`Constant`)
The constant pool stores values that cannot fit into immediate instruction fields:
- `Tag 0x00`: `Nil`
- `Tag 0x01`: `Bool(false)`
- `Tag 0x02`: `Bool(true)`
- `Tag 0x03`: `Int(i64)` followed by 8 bytes.
- `Tag 0x04`: `Float(f64)` followed by 8 bytes.
- `Tag 0x05`: `String` followed by length-prefixed UTF-8 bytes.
- `Tag 0x06`: `BigInt` followed by signed magnitude bytes.

### 2.3 Upvalue Descriptors (`UpvalueDesc`)
Closures capture lexical state via upvalues:
```text
+-------------------+-----------------+
| is_local (1 byte) | index (1 byte)  |
+-------------------+-----------------+
```
- `is_local == 1`: The upvalue captures a local variable residing at register `index` in the enclosing call frame.
- `is_local == 0`: The upvalue captures an upvalue at position `index` from the enclosing prototype's upvalue array.

### 2.4 Prototype (`Proto`)
Each function (including the root main chunk) is serialized as a `Proto`:
```text
+--------------------------------------------------------+
| Name (Option<String>): Tag (u8: 0=None, 1=Some) + String|
| Source path (Option<String>)                           |
| num_params (u8)                                        |
| is_vararg (u8: 0=false, 1=true)                        |
| max_registers (u8)                                     |
| import_cache_size (u16)                                |
| Constants Count (u32) + Constants array                |
| Upvalues Count (u32) + Upvalues array                  |
| Code Instructions Count (u32) + Instructions array     |
| Line Info Count (u32) + Line numbers array (u32 each)  |
| Child Protos Count (u32) + Child Protos array          |
+--------------------------------------------------------+
```

---

## 3. Register Machine Architecture

Neyuki is a register-based virtual machine:
- Registers `R(0)` through `R(max_registers - 1)` form the local execution window for a call frame.
- `R(0)` through `R(num_params - 1)` hold incoming function arguments.
- Intermediate computation results, table references, and temporary values are addressed by register index.
- Special constant sentinel `MULTRET = 255`: Indicates that an instruction consumes or emits variable length returns up to the dynamic stack top.

---

## 4. Opcode Reference

All instructions are serialized as a 1-byte opcode tag followed by operand fields.

### 4.1 Data Transfer and Constants
| Opcode Tag | Mnemonic | Operands | Operational Semantics |
|:---|:---|:---|:---|
| `0x00` | `LoadNil` | `dst: u8` | `R(dst) = nil` |
| `0x01` | `LoadBool` | `dst: u8, val: u8` | `R(dst) = bool(val)` |
| `0x02` | `LoadInt` | `dst: u8, val: i32` | `R(dst) = val` (sign-extended 32-bit int) |
| `0x03` | `LoadK` | `dst: u8, k: u16` | `R(dst) = Constants[k]` |
| `0x04` | `Move` | `dst: u8, src: u8` | `R(dst) = R(src)` |

### 4.2 Global and Upvalue Access
| Opcode Tag | Mnemonic | Operands | Operational Semantics |
|:---|:---|:---|:---|
| `0x05` | `GetGlobal` | `dst: u8, name_k: u16` | `R(dst) = Globals[Constants[name_k]]` |
| `0x06` | `SetGlobal` | `src: u8, name_k: u16` | `Globals[Constants[name_k]] = R(src)` |
| `0x07` | `GetUpval` | `dst: u8, upval_idx: u8` | `R(dst) = Upvalues[upval_idx]` |
| `0x08` | `SetUpval` | `src: u8, upval_idx: u8` | `Upvalues[upval_idx] = R(src)` |

### 4.3 Table Operations
| Opcode Tag | Mnemonic | Operands | Operational Semantics |
|:---|:---|:---|:---|
| `0x09` | `NewTable` | `dst: u8` | `R(dst) = {}` |
| `0x0A` | `GetTable` | `dst: u8, table: u8, key: u8` | `R(dst) = R(table)[R(key)]` |
| `0x0B` | `SetTable` | `table: u8, key: u8, val: u8` | `R(table)[R(key)] = R(val)` |
| `0x0C` | `GetTableK` | `dst: u8, table: u8, key_k: u16` | `R(dst) = R(table)[Constants[key_k]]` |
| `0x0D` | `SetTableK` | `table: u8, key_k: u16, val: u8` | `R(table)[Constants[key_k]] = R(val)` |
| `0x0E` | `AppendArray`| `table: u8, src: u8` | `R(table).push(R(src))` |
| `0x0F` | `GetImport` | `dst: u8, mod_k: u16, field_k: u16, site: u16` | Cached module field lookup |
| `0x10` | `SetList` | `table: u8, base: u8, count: u8` | Appends `count` registers `R(base)..` to `R(table)` |

### 4.4 Arithmetic and Bitwise Instructions
| Opcode Tag | Mnemonic | Operands | Operational Semantics |
|:---|:---|:---|:---|
| `0x11` | `Add` | `dst: u8, a: u8, b: u8` | `R(dst) = R(a) + R(b)` |
| `0x12` | `Sub` | `dst: u8, a: u8, b: u8` | `R(dst) = R(a) - R(b)` |
| `0x13` | `Mul` | `dst: u8, a: u8, b: u8` | `R(dst) = R(a) * R(b)` |
| `0x14` | `Div` | `dst: u8, a: u8, b: u8` | `R(dst) = R(a) / R(b)` (float division) |
| `0x15` | `IDiv` | `dst: u8, a: u8, b: u8` | `R(dst) = R(a) // R(b)` (integer division) |
| `0x16` | `Mod` | `dst: u8, a: u8, b: u8` | `R(dst) = R(a) % R(b)` |
| `0x17` | `Pow` | `dst: u8, a: u8, b: u8` | `R(dst) = R(a) ^ R(b)` |
| `0x18` | `BitAnd` | `dst: u8, a: u8, b: u8` | `R(dst) = R(a) & R(b)` |
| `0x19` | `BitOr` | `dst: u8, a: u8, b: u8` | `R(dst) = R(a) \| R(b)` |
| `0x1A` | `BitXor` | `dst: u8, a: u8, b: u8` | `R(dst) = R(a) ~ R(b)` |
| `0x1B` | `Shl` | `dst: u8, a: u8, b: u8` | `R(dst) = R(a) << R(b)` |
| `0x1C` | `Shr` | `dst: u8, a: u8, b: u8` | `R(dst) = R(a) >> R(b)` (arithmetic shift) |
| `0x1D` | `LShl` | `dst: u8, a: u8, b: u8` | `R(dst) = R(a) <<< R(b)` (logical shift) |
| `0x1E` | `LShr` | `dst: u8, a: u8, b: u8` | `R(dst) = R(a) >>> R(b)` (logical shift) |
| `0x1F` | `Concat` | `dst: u8, a: u8, b: u8` | `R(dst) = R(a) .. R(b)` |
| `0x20` | `Unm` | `dst: u8, src: u8` | `R(dst) = -R(src)` |
| `0x21` | `Not` | `dst: u8, src: u8` | `R(dst) = not R(src)` |
| `0x22` | `Len` | `dst: u8, src: u8` | `R(dst) = #R(src)` |
| `0x23` | `BitNot` | `dst: u8, src: u8` | `R(dst) = ~R(src)` |
| `0x24` | `Coalesce` | `dst: u8, a: u8, b: u8` | `R(dst) = R(a) ?? R(b)` |

### 4.5 Branching and Control Flow
| Opcode Tag | Mnemonic | Operands | Operational Semantics |
|:---|:---|:---|:---|
| `0x25` | `Eq` | `a: u8, b: u8, jump: i16` | If `R(a) != R(b)` jump `ip += jump` |
| `0x26` | `Ne` | `a: u8, b: u8, jump: i16` | If `R(a) == R(b)` jump `ip += jump` |
| `0x27` | `Lt` | `a: u8, b: u8, jump: i16` | If `!(R(a) < R(b))` jump `ip += jump` |
| `0x28` | `Le` | `a: u8, b: u8, jump: i16` | If `!(R(a) <= R(b))` jump `ip += jump` |
| `0x29` | `Gt` | `a: u8, b: u8, jump: i16` | If `!(R(a) > R(b))` jump `ip += jump` |
| `0x2A` | `Ge` | `a: u8, b: u8, jump: i16` | If `!(R(a) >= R(b))` jump `ip += jump` |
| `0x2B` | `Test` | `reg: u8, jump: i16` | If `!is_truthy(R(reg))` jump `ip += jump` |
| `0x2C` | `Jump` | `offset: i16` | Unconditional jump `ip += offset` |
| `0x2D` | `ForPrep` | `base: u8, jump: i16` | Loop init: `R(base) -= R(base+2)`; jump |
| `0x2E` | `ForLoop` | `base: u8, jump: i16` | Loop step: `R(base) += R(base+2)`; test & jump |
| `0x2F` | `TForCall`| `base: u8, retc: u8` | Generic iterator call |
| `0x30` | `TForLoop`| `base: u8, jump: i16` | Generic loop test & jump |

### 4.6 Function Invocation and Closures
| Opcode Tag | Mnemonic | Operands | Operational Semantics |
|:---|:---|:---|:---|
| `0x31` | `Call` | `callee: u8, argc: u8, retc: u8` | Call `R(callee)` with `argc` args, saving `retc` results |
| `0x32` | `Return` | `base: u8, count: u8` | Return `count` values starting at `R(base)` |
| `0x33` | `Closure` | `dst: u8, proto_idx: u16` | Create closure from child prototype `proto_idx` |
| `0x34` | `Vararg` | `dst: u8, count: u8` | Copy `count` vararg values into `R(dst)..` |

### 4.7 Immediate Integer Instructions (Format 7+)
Constant-operand ops `AddK`/`SubK`/`MulK` (tags `66..=68`, `dst, a, k:u16`) and `EqK`/`NeK`/`LtK`/`LeK`/`GtK`/`GeK` (tags `69..=74`, `a, k:u16, jump_if_false:i16`) were added in `BYTECODE_VERSION = 8`; `k` indexes the constant pool.

Added in `BYTECODE_VERSION = 7` (wire tags `55..=65` / `0x37..=0x41`; format 6 files continue to load without these tags). All immediate operands preserve full dynamic/metamethod semantics by falling back to `Value::Int(imm)` for non-primitive or table operands.

| Wire Tag | Mnemonic | Operands | Operational Semantics |
|:---|:---|:---|:---|
| `55` (`0x37`) | `AddI` | `dst: u8, a: u8, imm: i16` | `R(dst) = R(a) + imm` |
| `56` (`0x38`) | `SubI` | `dst: u8, a: u8, imm: i16` | `R(dst) = R(a) - imm` |
| `57` (`0x39`) | `MulI` | `dst: u8, a: u8, imm: i16` | `R(dst) = R(a) * imm` |
| `58` (`0x3A`) | `ShlI` | `dst: u8, a: u8, imm: u8` | `R(dst) = R(a) << imm` (`imm <= 63`) |
| `59` (`0x3B`) | `ShrI` | `dst: u8, a: u8, imm: u8` | `R(dst) = R(a) >> imm` (`imm <= 63`) |
| `60` (`0x3C`) | `EqI` | `a: u8, imm: i16, jump: i16` | If `R(a) != imm` jump `ip += jump` |
| `61` (`0x3D`) | `NeI` | `a: u8, imm: i16, jump: i16` | If `R(a) == imm` jump `ip += jump` |
| `62` (`0x3E`) | `LtI` | `a: u8, imm: i16, jump: i16` | If `!(R(a) < imm)` jump `ip += jump` |
| `63` (`0x3F`) | `LeI` | `a: u8, imm: i16, jump: i16` | If `!(R(a) <= imm)` jump `ip += jump` |
| `64` (`0x40`) | `GtI` | `a: u8, imm: i16, jump: i16` | If `!(R(a) > imm)` jump `ip += jump` |
| `65` (`0x41`) | `GeI` | `a: u8, imm: i16, jump: i16` | If `!(R(a) >= imm)` jump `ip += jump` |

---

## 5. Bytecode Verification

Before execution, prototypes undergo linear verification (`verify_proto`):
1. **Register Bounds**: Every register operand must be strictly less than `max_registers`.
2. **Jump Offsets**: All jump instructions (`Jump`, `Test`, `Eq`...`Ge`, `EqI`...`GeI`, `ForPrep`, `ForLoop`, `TForLoop`) must resolve within the instruction boundaries of the enclosing prototype.
3. **Constant Indexes**: Every `k`, `name_k`, `key_k`, and `mod_k` must be strictly less than the length of `constants`.
4. **Child Prototype References**: Every `Closure` instruction must reference a valid child prototype in `protos`.
5. **Upvalue Indexes**: Every upvalue descriptor must point to a valid register in the parent or upvalue in the parent's upvalues array.
