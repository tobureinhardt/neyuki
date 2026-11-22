#![allow(dead_code)]

// Fuzzes the standard libraries with malformed input: json, string, utf8,
// tonumber, crypto and the sandbox.

use crate::compiler::{compile_source, compile_to_proto};
use crate::vm::libs::http::parse_multipart_bytes;
use crate::vm::libs::json::decode_from_str;
use crate::vm::libs::markdown::{
    parse_from_str as parse_md_from_str, to_html_from_str as render_md_from_str,
};
use crate::vm::machine::VM;
use crate::vm::value::Value;

fn execute_source(source: &str) -> Result<Value, String> {
    let stmts = compile_source(source)?;
    let proto = compile_to_proto(&stmts);
    let mut vm = VM::new();
    vm.execute(proto)
}

pub fn fuzz_json_parser_and_nesting() {
    // 1. Deeply nested JSON arrays beyond depth limit (256)
    for depth in [50, 100, 200, 255, 260, 300] {
        let mut json = "1".to_string();
        for _ in 0..depth {
            json = format!("[{}]", json);
        }
        let res = decode_from_str(&json);
        if depth > 256 {
            assert!(
                res.is_err(),
                "JSON nesting depth {} must be rejected by depth limit",
                depth
            );
        } else {
            assert!(
                res.is_ok(),
                "JSON nesting depth {} should decode cleanly",
                depth
            );
        }
    }

    // 2. Malformed JSON strings
    let malformed_json = [
        "",
        "{",
        "}",
        "[",
        "]",
        "{ \"key\": }",
        "{ \"key\": 123, }", // trailing comma
        "[1, 2, 3, ]",       // trailing comma
        "\"unclosed string",
        "\"escaped quote \\\"",
        "{\"a\": {\"b\": [1, 2, {\"c\": 3}}",
        "true false",
        "12.34.56",
        "{\"\\uZZZZ\": 1}",
        "{\"\\uD800\": 1}",
        "undefined",
        "NaN",
        "Infinity",
    ];

    for s in &malformed_json {
        let res = decode_from_str(s);
        assert!(res.is_err(), "malformed JSON '{}' must be rejected", s);
    }

    // 3. Roundtrip encode & decode
    let roundtrip_script = r#"
        local json = require("@neyuki/json")
        local orig = { a = 1, b = "test", c = { true, false, nil, 3.14 } }
        local enc = json.encode(orig)
        local dec = json.decode(enc)
        assert(dec.a == 1)
        assert(dec.b == "test")
        assert(dec.c[1] == true)
        assert(dec.c[2] == false)
    "#;
    assert!(execute_source(roundtrip_script).is_ok());
}

pub fn fuzz_string_and_utf8_edge_cases() {
    // 1. string.sub with extreme, negative, and inverted indices
    let sub_script = r#"
        local string = require("@neyuki/string")
        local s = "Hello, Neyuki!"
        assert(string.sub(s, 1, 5) == "Hello")
        assert(string.sub(s, -7, -2) == "Neyuki")
        assert(string.sub(s, 10, 5) == "") -- inverted indices
        assert(string.sub(s, 100, 200) == "") -- out of range
        assert(string.sub(s, -500, -100) == "") -- negative out of range
    "#;
    let res = execute_source(sub_script);
    assert!(res.is_ok(), "failed: {:?}", res.err());

    // 2. UTF-8 multi-byte characters and utf8.char
    let utf8_script = r#"
        local utf8 = require("@neyuki/utf8")
        local string = require("@neyuki/string")
        local vi = "Xin chào thế giới 🚀"
        assert(utf8.len(vi) == 19)
        local c1 = utf8.char(65) -- 'A'
        assert(c1 == "A")
        local c2 = utf8.char(128512) -- 0x1F680: 🚀
        assert(utf8.len(c2) == 1)
    "#;
    let res = execute_source(utf8_script);
    assert!(res.is_ok(), "utf8_script error: {:?}", res.err());

    // 3. utf8.char with invalid codepoints (surrogates, out of bounds)
    let bad_utf8_script = r#"
        local utf8 = require("@neyuki/utf8")
        assert(pcall(utf8.char, 55296) == false) -- 0xD800: surrogate
        assert(pcall(utf8.char, 57343) == false) -- 0xDFFF: surrogate
        assert(pcall(utf8.char, 1114112) == false) -- 0x110000: out of range
        assert(pcall(utf8.char, -1) == false) -- negative
    "#;
    assert!(execute_source(bad_utf8_script).is_ok());

    // 4. string.rep huge repetition count protection
    let rep_script = r#"
        local string = require("@neyuki/string")
        assert(string.rep("a", 0) == "")
        assert(string.rep("a", -5) == "")
        -- Huge repetition exceeding limits must fail with error, not OOM panic
        local ok, _ = pcall(string.rep, "hello", 100000000)
        assert(ok == false)
    "#;
    assert!(execute_source(rep_script).is_ok());

    // 5. string.format specifier fuzzing
    let format_script = r#"
        local string = require("@neyuki/string")
        assert(string.format("num: %d, str: %s, hex: %x", 42, "abc", 255) == "num: 42, str: abc, hex: ff")
        assert(string.format("percent: %%") == "percent: %")
        assert(string.format("hex: %X", 255) == "hex: FF")
    "#;
    let res = execute_source(format_script);
    assert!(res.is_ok(), "format error: {:?}", res.err());
}

pub fn fuzz_number_parsing_and_radix() {
    // 1. tonumber bases 2..=36
    let tonumber_script = r#"
        assert(tonumber("1010", 2) == 10)
        assert(tonumber("ff", 16) == 255)
        assert(tonumber("0xFF", 16) == 255)
        assert(tonumber("z", 36) == 35)
        assert(tonumber("-101", 2) == -5)
        assert(tonumber("   +42   ") == 42)
        assert(tonumber("   -42   ") == -42)
        assert(tonumber("1e3") == 1000.0)

        -- Invalid bases must be rejected
        assert(pcall(tonumber, "10", 1) == false)
        assert(pcall(tonumber, "10", 37) == false)
        assert(pcall(tonumber, "10", -5) == false)

        -- Invalid digits for base must return nil
        assert(tonumber("12", 2) == nil)
        assert(tonumber("xyz", 10) == nil)
    "#;
    assert!(execute_source(tonumber_script).is_ok());

    // 2. Length cap check on tonumber (string > 65,536 bytes)
    let huge_str = "1".repeat(70_000);
    let cap_script = format!("assert(tonumber(\"{}\") == nil)", huge_str);
    assert!(execute_source(&cap_script).is_ok());
}

pub fn fuzz_crypto_boundaries() {
    // SHA-256 padding boundaries (55, 56, 63, 64, 65, 128 bytes) through the
    // bundled module, plus a few known answers.
    for len in [0, 1, 55, 56, 63, 64, 65, 119, 120, 127, 128, 512, 1024] {
        let script = format!(
            "local crypto = require(\"@neyuki/crypto\")
return crypto.hash(string.rep(\"a\", {}))",
            len
        );
        let hash = execute_source(&script).expect("hash should succeed");
        assert_eq!(
            hash.to_string().len(),
            64,
            "SHA-256 hash must be 64 hex characters"
        );
    }

    let crypto_script = r#"
        local crypto = require("@neyuki/crypto")
        assert(crypto.hash("") == "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        assert(crypto.hash("abc", "sha256") == "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")

        -- Unknown algorithms and encodings must error, not panic
        assert(pcall(crypto.hash, "abc", "rot13") == false)
        assert(pcall(crypto.hash, "abc", "sha256", "base32") == false)

        -- Malformed ciphertexts, keys and signatures must error, not panic
        local key = crypto.generateKey()
        for _, bad in {"", "!", "AAAA", crypto.randomBytes(5), crypto.randomBytes(64)} do
            assert(pcall(crypto.decrypt, bad, key) == false)
            assert(pcall(crypto.encrypt, "x", bad) == false)
        end
        local pair = crypto.generateKeyPair()
        for _, bad in {"", "!", "AAAA", crypto.randomBytes(64), crypto.randomBytes(100)} do
            assert(crypto.verify("x", bad, pair.publicKey) == false)
        end
        for _, bad in {"", "-----BEGIN PUBLIC KEY-----
AAAA
-----END PUBLIC KEY-----
", pair.publicKey .. "x"} do
            assert(pcall(crypto.sign, "x", bad) == false)
            assert(pcall(crypto.verify, "x", "AAAA", bad) == false)
        end
        assert(pcall(crypto.verifyPassword, "x", "$argon2id$garbage") == false)
        assert(pcall(crypto.verifyPassword, "x", "$2b$garbage") == false)
        assert(pcall(crypto.verifyPassword, "x", "plain") == false)
    "#;
    assert!(execute_source(crypto_script).is_ok());
}

pub fn fuzz_markdown_parser_and_xss() {
    // 1. Pathological delimiter and nesting inputs must run linearly and respect depth limits
    let deep_quotes = "> ".repeat(300) + "deep";
    assert!(
        parse_md_from_str(&deep_quotes).is_err(),
        "deeply nested blockquotes must hit depth limit"
    );

    let bracket_storm = "[".repeat(4_000) + "x" + &"]".repeat(4_000);
    assert!(render_md_from_str(&bracket_storm).is_ok());

    let star_storm = "*a ".repeat(2_000);
    assert!(render_md_from_str(&star_storm).is_ok());

    // 2. Default safe HTML must neutralize raw HTML and dangerous URL schemes
    let xss = "<script>alert(1)</script>\n\nHi <img src=x onerror=alert(1)>\n\n[bad](javascript:alert(1))\n\n[b2](<JaVa\tScRiPt:1>)\n\n![img](data:text/html,hi)";
    let rendered = render_md_from_str(xss).expect("safe render should succeed");
    assert!(!rendered.contains("<script>"));
    assert!(rendered.contains("&lt;script&gt;"));
    assert!(!rendered.contains("<img src=x"));
    assert!(rendered.contains("&lt;img "));
    assert!(!rendered.contains("javascript:"));
    assert!(!rendered.contains("ScRiPt:"));
    assert!(!rendered.contains("data:text/html"));
}

pub fn fuzz_http_multipart_and_cookies() {
    // 1. Malformed multipart payloads must error cleanly, never panic
    let bad_cases: &[(&[u8], &str)] = &[
        (b"", "multipart/form-data; boundary=abc"),
        (b"--abc\r\n", "text/plain"),
        (b"--abc\r\n", "multipart/form-data"),
        (b"--abc\r\n", "multipart/form-data; boundary="),
        (
            b"--abc\r\nContent-Disposition: form-data\r\n\r\nval\r\n--abc--",
            "multipart/form-data; boundary=abc",
        ),
        (
            b"--abc\r\nContent-Disposition: form-data; name=\"x\"\r\n\r\nunterminated",
            "multipart/form-data; boundary=abc",
        ),
        (
            b"--abc\r\nBadHeaderNoColon\r\n\r\nval\r\n--abc--",
            "multipart/form-data; boundary=abc",
        ),
    ];
    for (body, ct) in bad_cases {
        assert!(
            parse_multipart_bytes(body, ct).is_err(),
            "malformed multipart ({}) must be rejected",
            ct
        );
    }

    // 2. Cookie & redirect CRLF injection guards through @neyuki/http
    let cookie_script = r#"
        local http = require("@neyuki/http")
        assert(pcall(http.formatCookie, "bad;name", "v") == false)
        assert(pcall(http.formatCookie, "ok", "bad;val") == false)
        assert(pcall(http.formatCookie, "ok", "v", { path = "/\r\nX-Injected: 1" }) == false)
        assert(pcall(http.redirect, "/ok\r\nSet-Cookie: evil=1") == false)
        assert(http.unsignCookie("tampered.sig", "secret") == nil)
    "#;
    assert!(execute_source(cookie_script).is_ok());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_libs_fuzz_all() {
        fuzz_json_parser_and_nesting();
        fuzz_string_and_utf8_edge_cases();
        fuzz_number_parsing_and_radix();
        fuzz_crypto_boundaries();
        fuzz_markdown_parser_and_xss();
        fuzz_http_multipart_and_cookies();
    }
}
