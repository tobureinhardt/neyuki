//! Random input to the standard library's parsers and decoders: JSON,
//! `string.format`, number parsing, UTF-8, patterns, `string.pack`, crypto
//! encodings and keys, and buffers.
//!
//! An input is one selector byte, two little-endian i64 arguments and a
//! payload string, handed to the script the selector picks as `input`, `n1`
//! and `n2`.

use rand::Rng;
use rand::rngs::StdRng;

use super::super::corpus::{Corpus, INTERESTING_U64, mutate_bytes, sandbox_vm};
use super::super::driver::Target;
use crate::compiler::{Options, compile_program};
use crate::vm::value::Value;

pub const TARGET: Target = Target {
    name: "libs",
    extension: "bin",
    default_seconds: 45,
    regressions,
    generate,
    run,
};

fn regressions() {
    use super::lib_fuzz::*;
    fuzz_json_parser_and_nesting();
    fuzz_string_and_utf8_edge_cases();
    fuzz_number_parsing_and_radix();
    fuzz_crypto_boundaries();
}

/// One script per selector. Each wraps its calls in `pcall`: errors are
/// expected, only panics and hangs are failures. `FUZZ-ORACLE` errors mark
/// broken guarantees and are turned into panics.
const SCRIPTS: &[&str] = &[
    // 0: JSON, and whatever it encodes back must decode again.
    "local ok, v = pcall(json.decode, input)
    if ok then
        local eok, s = pcall(json.encode, v)
        if eok and not pcall(json.decode, s) then error(\"FUZZ-ORACLE: json.encode output does not decode: \" .. s) end
    end",
    // 1: format directives from the payload.
    "pcall(string.format, input, n1, n2, input)
    pcall(string.format, input, input)
    pcall(string.format, \"%\" .. string.sub(input, 1, 6), n1)
    pcall(string.format, \"%\" .. string.sub(input, 1, 6), n1 / 7)",
    // 2: number parsing in every base.
    "pcall(tonumber, input)
    pcall(tonumber, input, n1 % 40)
    pcall(tonumber, input, n1)
    pcall(int, input)
    pcall(float, input)
    local ok, x = pcall(tonumber, input)
    if ok and x then pcall(string.format, \"%.17g %d %x\", x, x, x) pcall(math.floor, x) end",
    // 3: UTF-8.
    "pcall(utf8.len, input)
    pcall(utf8.len, input, n1, n2)
    pcall(utf8.codepoint, input, 1, -1)
    pcall(utf8.codepoint, input, n1, n2)
    pcall(utf8.offset, input, n1, n2)
    pcall(utf8.sub, input, n1, n2)
    pcall(utf8.char, n1, n2)
    pcall(function() for _, c in utf8.codes(input) do end end)
    pcall(function() for c in utf8.chars(input) do end end)",
    // 4: patterns, short enough that backtracking stays bounded.
    "local subject = string.sub(input, 1, 32)
    local pattern = string.sub(input, 33, 40)
    pcall(string.find, subject, pattern)
    pcall(string.find, subject, pattern, n1)
    pcall(string.match, subject, pattern)
    pcall(string.gsub, subject, pattern, \"%0%1\")
    pcall(string.gsub, subject, pattern, input, n1)
    pcall(function() for a, b in string.gmatch(subject, pattern) do end end)",
    // 5: crypto encodings and keys.
    "local crypto = require(\"@neyuki/crypto\")
    for _, enc in {\"hex\", \"base64\", \"base64url\", input} do
        pcall(crypto.decode, input, enc)
        pcall(crypto.encode, input, enc)
    end
    pcall(crypto.publicKey, input)
    pcall(crypto.verify, \"message\", input, KEY_PUBLIC)
    pcall(crypto.verify, \"message\", SIGNATURE, input)
    pcall(crypto.decrypt, input, KEY_SECRET)
    pcall(crypto.sharedSecret, input, KEY_PUBLIC)
    pcall(crypto.privateDecrypt, input, input)
    pcall(crypto.hash, input, input)",
    // 6: string.pack formats.
    "local fmt = string.sub(input, 1, 12)
    pcall(string.packsize, fmt)
    pcall(string.pack, fmt, n1, n2, input)
    pcall(string.unpack, fmt, string.sub(input, 13))
    pcall(string.unpack, fmt, string.sub(input, 13), n1)",
    // 7: buffers at hostile offsets.
    "local b = buffer.fromstring(input)
    for _, name in {\"readu8\", \"readi16\", \"readu32\", \"readi64\", \"readu64\", \"readf32\", \"readf64\"} do
        pcall(buffer[name], b, n1)
        pcall(buffer[name], b, n2, true)
        pcall(buffer[string.gsub(name, \"read\", \"write\")], b, n1, n2)
    end
    pcall(buffer.readbits, b, n1, n2 % 40)
    pcall(buffer.writebits, b, n1, n2 % 40, n1)
    pcall(buffer.readstring, b, n1, n2)
    pcall(buffer.writestring, b, n1, input, n2)
    pcall(buffer.slice, b, n1, n2)
    pcall(buffer.copy, b, n1, b, n2, #input)
    pcall(buffer.fill, b, n1, 7, n2)
    pcall(buffer.fromhex, input)
    pcall(buffer.frombase64, input)
    pcall(buffer.equal, b, buffer.fromstring(string.reverse(input)))",
    // 8: string functions with hostile indices.
    "pcall(string.sub, input, n1, n2)
    pcall(string.byte, input, n1, n2)
    pcall(string.char, n1, n2)
    pcall(string.rep, string.sub(input, 1, 8), n1 % 64, input)
    pcall(string.split, input, string.sub(input, 1, 2))
    pcall(string.count, input, string.sub(input, 1, 2))
    pcall(string.startswith, input, input)
    pcall(table.concat, {input, input}, input, n1, n2)
    pcall(table.unpack, {1, 2, 3}, n1, n1 + n1 % 5)",
    // 9: integer and float arithmetic on the arguments.
    "for _, f in {
        function() return n1 // n2 end, function() return n1 % n2 end,
        function() return n1 << n2 end, function() return n1 >> n2 end,
        function() return n1 * n2 end, function() return n1 ^ n2 end,
        function() return -n1 end, function() return n1 + n2 end,
        function() return math.fmod(n1, n2) end, function() return math.abs(n1) end,
        function() return bit.lshift(n1, n2) end, function() return bit.extract(n1, n2 % 64) end,
        function() return tostring(n1 / n2) end, function() return math.tointeger(n1 / 3) end,
    } do pcall(f) end",
];

/// Payload seeds per selector.
const SEEDS: &[&[&str]] = &[
    &[
        "{\"a\": [1, 2.5, -3e10, true, null, \"x\\u00e9\\n\"], \"b\": {}}",
        "[[[[[[[[[[[]]]]]]]]]]]",
        "\"\\ud83d\\ude00\\ud800\"",
        "12345678901234567890123",
        "-0.0e-999",
        "{\"\":{\"\":[]}}",
    ],
    &[
        "%d %s %5.2f %q %x %c %e %g %a %%",
        "%-+ #0123.456789d",
        "%*d",
        "%.99999f",
        "%s%s%s%s",
    ],
    &[
        "0x1p4",
        "  -0x7FFFFFFFFFFFFFFF  ",
        "1e309",
        "9223372036854775808",
        "0b101",
        "z",
        "1_000",
        ".e1",
    ],
    &[
        "héllo wörld \u{1F600}",
        "\u{FEFF}\u{10FFFF}\u{0}",
        "a\u{301}\u{302}",
        "abc",
    ],
    &[
        "key=value; other=thing                (%w+)=(%w+)",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa*a*a*b",
        "((()))  %b()",
        "[%]   [^%a-z]",
    ],
    &[
        "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEAGb9ECWmEzf6FQbrBZ9w7lshQhqowtrbLDFw4rXAxZuE=\n-----END PUBLIC KEY-----\n",
        "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA==\n-----END PRIVATE KEY-----\n",
        "-----BEGIN RSA PRIVATE KEY-----\nMIIBOgIBAAJBAKj34GkxFhD90vcNLYLInFEX6Ppy1tPf9Cnzj4p4WGeKLs1Pt8Qu\n-----END RSA PRIVATE KEY-----\n",
        "deadbeef",
        "SGVsbG8=",
        "SGVsbG8_-w",
    ],
    &[
        "<i4>I8=!8xXc5s1z",
        "i16",
        "c4096",
        "s9z",
        "<d>f>n>j",
        "!17",
        "Xi8",
    ],
    &[
        "\u{0}\u{1}\u{2}\u{3}\u{4}\u{5}\u{6}\u{7}",
        "0123456789abcdef",
        "SGVsbG8=",
    ],
    &["a,b,,c", "", "ab"],
    &[""],
];

fn generate(_corpus: &Corpus, rng: &mut StdRng) -> Vec<u8> {
    let selector = rng.gen_range(0..SCRIPTS.len());
    let mut bytes = vec![selector as u8];
    for _ in 0..2 {
        let n: i64 = match rng.gen_range(0..4) {
            0 => INTERESTING_U64[rng.gen_range(0..INTERESTING_U64.len())] as i64,
            1 => (INTERESTING_U64[rng.gen_range(0..INTERESTING_U64.len())] as i64).wrapping_neg(),
            2 => rng.gen_range(-70..70),
            _ => rng.r#gen(),
        };
        bytes.extend_from_slice(&n.to_le_bytes());
    }
    let seeds = SEEDS[selector];
    let mut payload = seeds[rng.gen_range(0..seeds.len())].as_bytes().to_vec();
    let other = seeds[rng.gen_range(0..seeds.len())].as_bytes();
    if rng.gen_ratio(4, 5) {
        mutate_bytes(rng, &mut payload, other);
    }
    bytes.extend(payload);
    bytes
}

fn run(input: &[u8]) {
    let Some((&selector, rest)) = input.split_first() else {
        return;
    };
    let Some(script) = SCRIPTS.get(selector as usize % SCRIPTS.len().max(1)) else {
        return;
    };
    let mut args = [0i64; 2];
    for (i, arg) in args.iter_mut().enumerate() {
        if let Some(chunk) = rest.get(i * 8..i * 8 + 8) {
            *arg = i64::from_le_bytes(chunk.try_into().unwrap());
        }
    }
    let payload = String::from_utf8_lossy(rest.get(16..).unwrap_or_default());

    let mut vm = sandbox_vm();
    vm.set_global("input", Value::str(&payload));
    vm.set_global("n1", Value::int(args[0]));
    vm.set_global("n2", Value::int(args[1]));
    if selector as usize % SCRIPTS.len() == 5 {
        set_keys(&mut vm);
    }
    let proto =
        compile_program(script, "fuzz", Options::UNCHECKED).expect("fuzz library scripts compile");
    if let Err(err) = vm.execute(proto)
        && err.contains("FUZZ-ORACLE")
    {
        panic!("{}", err);
    }
}

/// Gives the crypto script a real key, secret and signature to check
/// mangled ones against.
fn set_keys(vm: &mut crate::vm::machine::VM) {
    let setup = "local crypto = require(\"@neyuki/crypto\")
        local keys = crypto.generateKeyPair()
        KEY_PUBLIC = keys.publicKey
        KEY_SECRET = crypto.generateKey()
        SIGNATURE = crypto.sign(\"message\", keys.privateKey)";
    if let Ok(proto) = compile_program(setup, "fuzz", Options::UNCHECKED) {
        let _ = vm.execute(proto);
    }
}
