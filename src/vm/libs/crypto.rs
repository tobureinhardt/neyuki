//! Native cryptography primitives backing `@neyuki/crypto`.
//!
//! Everything here is a thin bridge to the RustCrypto and dalek
//! crates. They are used instead of Neyuki implementations so that secret
//! keys never leak through timing side channels and so that bulk encryption
//! runs at native speed. Values cross the boundary as strings: digests,
//! MACs, symmetric keys, ciphertexts and signatures are hex or base64 (the
//! `encoding` argument picks which) and asymmetric keys are PEM (PKCS#8 for
//! private keys, SPKI for public keys, so they interoperate with OpenSSL).
//! `lib/crypto.nyk` validates arguments and provides the user-facing API on
//! top of these.

use aes_gcm::aead::generic_array::typenum::Unsigned;
use aes_gcm::aead::{Aead, AeadCore, KeyInit, KeySizeUser, Nonce, Payload};
use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::{Engine, alphabet};
use ed25519_dalek::{SigningKey, VerifyingKey};
use hmac::{Hmac, Mac};
use pkcs8::der::asn1::{BitStringRef, OctetStringRef};
use pkcs8::der::{Decode, Encode, EncodePem};
use pkcs8::spki::{AlgorithmIdentifierRef, SubjectPublicKeyInfoRef};
use pkcs8::{
    DecodePrivateKey, DecodePublicKey, Document, EncodePrivateKey, EncodePublicKey, LineEnding,
    ObjectIdentifier, PrivateKeyInfo, SecretDocument,
};
use rand::rngs::OsRng;
use rand::{Rng, RngCore};
use rsa::pkcs1::{DecodeRsaPrivateKey, DecodeRsaPublicKey};
use rsa::signature::{RandomizedSigner, SignatureEncoding, Signer, Verifier};
use rsa::{Oaep, RsaPrivateKey, RsaPublicKey};
use sha2::Digest;
use subtle::ConstantTimeEq;
use x25519_dalek::StaticSecret;

use crate::vm::machine::VM;
use crate::vm::value::{NativeDef, Value};

pub(crate) const NATIVES: &[NativeDef] = &[
    NativeDef {
        name: "__crypto_hash",
        func: builtin_hash,
    },
    NativeDef {
        name: "__crypto_hmac",
        func: builtin_hmac,
    },
    NativeDef {
        name: "__crypto_equals",
        func: builtin_equals,
    },
    NativeDef {
        name: "__crypto_pbkdf2",
        func: builtin_pbkdf2,
    },
    NativeDef {
        name: "__crypto_hkdf",
        func: builtin_hkdf,
    },
    NativeDef {
        name: "__crypto_password_hash",
        func: builtin_password_hash,
    },
    NativeDef {
        name: "__crypto_password_verify",
        func: builtin_password_verify,
    },
    NativeDef {
        name: "__crypto_generate_key",
        func: builtin_generate_key,
    },
    NativeDef {
        name: "__crypto_encrypt",
        func: builtin_encrypt,
    },
    NativeDef {
        name: "__crypto_decrypt",
        func: builtin_decrypt,
    },
    NativeDef {
        name: "__crypto_generate_keypair",
        func: builtin_generate_keypair,
    },
    NativeDef {
        name: "__crypto_public_key",
        func: builtin_public_key,
    },
    NativeDef {
        name: "__crypto_sign",
        func: builtin_sign,
    },
    NativeDef {
        name: "__crypto_verify",
        func: builtin_verify,
    },
    NativeDef {
        name: "__crypto_public_encrypt",
        func: builtin_public_encrypt,
    },
    NativeDef {
        name: "__crypto_private_decrypt",
        func: builtin_private_decrypt,
    },
    NativeDef {
        name: "__crypto_shared_secret",
        func: builtin_shared_secret,
    },
    NativeDef {
        name: "__crypto_random_bytes",
        func: builtin_random_bytes,
    },
    NativeDef {
        name: "__crypto_random_int",
        func: builtin_random_int,
    },
    NativeDef {
        name: "__crypto_uuid",
        func: builtin_uuid,
    },
    NativeDef {
        name: "__crypto_encode",
        func: builtin_encode,
    },
    NativeDef {
        name: "__crypto_decode",
        func: builtin_decode,
    },
];

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

fn str_arg<'a>(args: &'a [Value], index: usize, name: &str) -> Result<&'a str, String> {
    match args.get(index) {
        Some(v) if v.is_string() => Ok(v.as_str().unwrap()),
        Some(value) => Err(format!(
            "{} must be a string, got {}",
            name,
            value.type_name()
        )),
        None => Err(format!("{} must be provided", name)),
    }
}

/// A string that may be absent; nil reads as the empty string.
fn optional_str_arg<'a>(args: &'a [Value], index: usize, name: &str) -> Result<&'a str, String> {
    match args.get(index) {
        None => Ok(""),
        Some(v) if v.is_nil() => Ok(""),
        _ => str_arg(args, index, name),
    }
}

fn integer_arg(args: &[Value], index: usize, name: &str) -> Result<i64, String> {
    let Some(value) = args.get(index) else {
        return Err(format!("{} must be provided", name));
    };
    match value.view() {
        crate::vm::value::ValueView::Int(v) => Ok(v),
        crate::vm::value::ValueView::BigInt(_) => Err(format!("{} is too large", name)),
        crate::vm::value::ValueView::Float(v) if v.fract() == 0.0 && v.is_finite() => Ok(v as i64),
        crate::vm::value::ValueView::Float(_) => Err(format!("{} must be an integer", name)),
        _ => Err(format!(
            "{} must be an integer, got {}",
            name,
            value.type_name()
        )),
    }
}

fn size_arg(args: &[Value], index: usize, name: &str) -> Result<usize, String> {
    let value = integer_arg(args, index, name)?;
    usize::try_from(value).map_err(|_| format!("{} must not be negative", name))
}

/// The most bytes a script may ask `randomBytes`, `pbkdf2` or `hkdf` for. A
/// larger request would abort the process when the allocation fails.
const MAX_OUTPUT_LENGTH: usize = 64 * 1024 * 1024;

/// The most memory, in KiB, argon2 may be asked to use (1 GiB).
const MAX_ARGON2_MEMORY: usize = 1024 * 1024;

/// An output length in bytes, at most `MAX_OUTPUT_LENGTH`.
fn length_arg(args: &[Value], index: usize, name: &str) -> Result<usize, String> {
    let length = size_arg(args, index, name)?;
    if length > MAX_OUTPUT_LENGTH {
        return Err(format!(
            "{} must be at most {} bytes",
            name, MAX_OUTPUT_LENGTH
        ));
    }
    Ok(length)
}

fn string_value(text: String) -> Vec<Value> {
    vec![Value::string(text)]
}

// ---------------------------------------------------------------------------
// Encodings
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Encoding {
    Hex,
    Base64,
    Base64Url,
}

// Decoding is padding-agnostic so that values produced elsewhere (JWTs drop
// the padding, most other tools keep it) are accepted either way.
const BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);
const BASE64_URL: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

fn encoding_arg(args: &[Value], index: usize) -> Result<Encoding, String> {
    match str_arg(args, index, "encoding")? {
        "hex" => Ok(Encoding::Hex),
        "base64" => Ok(Encoding::Base64),
        "base64url" => Ok(Encoding::Base64Url),
        other => Err(format!(
            "unknown encoding, '{}' (expected hex, base64 or base64url)",
            other
        )),
    }
}

fn encode(bytes: &[u8], encoding: Encoding) -> String {
    match encoding {
        Encoding::Hex => {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            let mut out = String::with_capacity(bytes.len() * 2);
            for &byte in bytes {
                out.push(HEX[(byte >> 4) as usize] as char);
                out.push(HEX[(byte & 0x0f) as usize] as char);
            }
            out
        }
        Encoding::Base64 => BASE64.encode(bytes),
        Encoding::Base64Url => BASE64_URL.encode(bytes),
    }
}

fn decode(text: &str, encoding: Encoding, name: &str) -> Result<Vec<u8>, String> {
    match encoding {
        Encoding::Hex => {
            let digits = text.as_bytes();
            if !digits.len().is_multiple_of(2) {
                return Err(format!("{} is not valid hex (odd length)", name));
            }
            digits
                .chunks(2)
                .map(|pair| {
                    let hi = (pair[0] as char).to_digit(16);
                    let lo = (pair[1] as char).to_digit(16);
                    match (hi, lo) {
                        (Some(hi), Some(lo)) => Ok((hi * 16 + lo) as u8),
                        _ => Err(format!("{} is not valid hex", name)),
                    }
                })
                .collect()
        }
        Encoding::Base64 => BASE64
            .decode(text)
            .map_err(|_| format!("{} is not valid base64", name)),
        Encoding::Base64Url => BASE64_URL
            .decode(text)
            .map_err(|_| format!("{} is not valid base64url", name)),
    }
}

fn builtin_encode(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let encoding = encoding_arg(args, 0)?;
    let data = str_arg(args, 1, "s")?;
    Ok(string_value(encode(data.as_bytes(), encoding)))
}

fn builtin_decode(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let encoding = encoding_arg(args, 0)?;
    let data = str_arg(args, 1, "s")?;
    let bytes = decode(data, encoding, "s")?;
    Ok(string_value(utf8(bytes, "decoded data")?))
}

/// Neyuki strings are UTF-8, so bytes that are not (a decoded random key,
/// say) cannot be returned as a string; the caller has to keep them encoded.
fn utf8(bytes: Vec<u8>, what: &str) -> Result<String, String> {
    String::from_utf8(bytes)
        .map_err(|_| format!("{} is not valid UTF-8; keep it hex or base64 encoded", what))
}

// ---------------------------------------------------------------------------
// Hashing, MACs and key derivation
// ---------------------------------------------------------------------------

/// Runs `$body` with `$D` bound to the digest type named by `$algorithm`,
/// limited to the digests that can also drive HMAC (BLAKE2 keys itself
/// natively and has no eager block interface for HMAC to wrap).
macro_rules! with_hmac_digest {
    ($algorithm:expr, $D:ident => $body:expr) => {
        match $algorithm {
            "md5" => {
                type $D = md5::Md5;
                $body
            }
            "sha1" => {
                type $D = sha1::Sha1;
                $body
            }
            "sha224" => {
                type $D = sha2::Sha224;
                $body
            }
            "sha256" => {
                type $D = sha2::Sha256;
                $body
            }
            "sha384" => {
                type $D = sha2::Sha384;
                $body
            }
            "sha512" => {
                type $D = sha2::Sha512;
                $body
            }
            "sha3-256" => {
                type $D = sha3::Sha3_256;
                $body
            }
            "sha3-512" => {
                type $D = sha3::Sha3_512;
                $body
            }
            other @ ("blake2b" | "blake2s") => Err(format!(
                "{} cannot be used with hmac-based functions",
                other
            )),
            other => Err(format!("unknown hash algorithm, '{}'", other)),
        }
    };
}

/// Runs `$body` with `$D` bound to any supported digest type.
macro_rules! with_digest {
    ($algorithm:expr, $D:ident => $body:expr) => {
        match $algorithm {
            "blake2b" => {
                type $D = blake2::Blake2b512;
                $body
            }
            "blake2s" => {
                type $D = blake2::Blake2s256;
                $body
            }
            other => with_hmac_digest!(other, $D => $body),
        }
    };
}

fn builtin_hash(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let algorithm = str_arg(args, 0, "algorithm")?;
    let data = str_arg(args, 1, "s")?;
    let encoding = encoding_arg(args, 2)?;
    with_digest!(algorithm, D => {
        Ok(string_value(encode(&D::digest(data.as_bytes()), encoding)))
    })
}

fn builtin_hmac(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let algorithm = str_arg(args, 0, "algorithm")?;
    let key = str_arg(args, 1, "key")?;
    let data = str_arg(args, 2, "s")?;
    let encoding = encoding_arg(args, 3)?;
    with_hmac_digest!(algorithm, D => {
        // HMAC accepts keys of any length, so this cannot fail.
        let mut mac = <Hmac<D> as Mac>::new_from_slice(key.as_bytes())
            .map_err(|err| format!("invalid hmac key: {}", err))?;
        mac.update(data.as_bytes());
        Ok(string_value(encode(&mac.finalize().into_bytes(), encoding)))
    })
}

/// Constant-time equality, for comparing MACs and tokens without leaking
/// how many leading bytes matched.
fn builtin_equals(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let a = str_arg(args, 0, "a")?;
    let b = str_arg(args, 1, "b")?;
    Ok(vec![Value::bool(a.as_bytes().ct_eq(b.as_bytes()).into())])
}

fn builtin_pbkdf2(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let algorithm = str_arg(args, 0, "algorithm")?;
    let password = str_arg(args, 1, "password")?;
    let salt = str_arg(args, 2, "salt")?;
    let iterations = size_arg(args, 3, "iterations")?;
    let length = length_arg(args, 4, "length")?;
    let encoding = encoding_arg(args, 5)?;
    if iterations == 0 {
        return Err("iterations must be at least 1".to_string());
    }
    let iterations =
        u32::try_from(iterations).map_err(|_| "iterations is too large".to_string())?;
    let mut out = vec![0u8; length];
    with_hmac_digest!(algorithm, D => {
        pbkdf2::pbkdf2_hmac::<D>(password.as_bytes(), salt.as_bytes(), iterations, &mut out);
        Ok(string_value(encode(&out, encoding)))
    })
}

fn builtin_hkdf(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let algorithm = str_arg(args, 0, "algorithm")?;
    let key = str_arg(args, 1, "key")?;
    let salt = optional_str_arg(args, 2, "salt")?;
    let info = optional_str_arg(args, 3, "info")?;
    let length = length_arg(args, 4, "length")?;
    let encoding = encoding_arg(args, 5)?;
    // The input is key material (a shared secret, a master key), which is
    // binary and so arrives encoded like every other key.
    let key = decode(key, encoding, "key")?;
    let mut out = vec![0u8; length];
    with_hmac_digest!(algorithm, D => {
        let salt = if salt.is_empty() { None } else { Some(salt.as_bytes()) };
        hkdf::Hkdf::<D>::new(salt, &key)
            .expand(info.as_bytes(), &mut out)
            .map_err(|_| "length is too large for hkdf (at most 255 hash lengths)".to_string())?;
        Ok(string_value(encode(&out, encoding)))
    })
}

// ---------------------------------------------------------------------------
// Password hashing
// ---------------------------------------------------------------------------

fn builtin_password_hash(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let algorithm = str_arg(args, 0, "algorithm")?;
    let password = str_arg(args, 1, "password")?;
    let cost = size_arg(args, 2, "cost")?;
    let memory = size_arg(args, 3, "memory")?;
    let parallelism = size_arg(args, 4, "parallelism")?;
    let hash = match algorithm {
        "argon2id" => {
            if memory > MAX_ARGON2_MEMORY {
                return Err(format!(
                    "argon2 memory must be at most {} KiB",
                    MAX_ARGON2_MEMORY
                ));
            }
            let params = argon2::Params::new(
                u32::try_from(memory).map_err(|_| "memory is too large".to_string())?,
                u32::try_from(cost).map_err(|_| "cost is too large".to_string())?,
                u32::try_from(parallelism).map_err(|_| "parallelism is too large".to_string())?,
                None,
            )
            .map_err(|err| format!("invalid argon2 parameters: {}", err))?;
            let hasher = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
            let salt = SaltString::generate(&mut OsRng);
            hasher
                .hash_password(password.as_bytes(), &salt)
                .map_err(|err| format!("argon2 failed: {}", err))?
                .to_string()
        }
        "bcrypt" => {
            let cost = u32::try_from(cost).map_err(|_| "cost is too large".to_string())?;
            if !(4..=31).contains(&cost) {
                return Err("bcrypt cost must be between 4 and 31".to_string());
            }
            // bcrypt only looks at the first 72 bytes of the password; anything
            // longer would silently verify against a prefix.
            if password.len() > 72 {
                return Err("bcrypt passwords must be at most 72 bytes".to_string());
            }
            bcrypt::hash(password, cost).map_err(|err| format!("bcrypt failed: {}", err))?
        }
        other => return Err(format!("unknown password hash algorithm, '{}'", other)),
    };
    Ok(string_value(hash))
}

/// Verifies against whichever scheme produced the hash, so a stored hash can
/// be checked without remembering how it was made.
fn builtin_password_verify(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let password = str_arg(args, 0, "password")?;
    let hash = str_arg(args, 1, "hash")?;
    let matches = if hash.starts_with("$argon2") {
        let parsed =
            PasswordHash::new(hash).map_err(|err| format!("invalid argon2 hash: {}", err))?;
        // The PHC parser accepts `$argon2id$salt` alone; without both parts
        // there is nothing to verify against, which should not read as
        // "wrong password".
        if parsed.salt.is_none() || parsed.hash.is_none() {
            return Err("invalid argon2 hash: missing salt or hash".to_string());
        }
        match Argon2::default().verify_password(password.as_bytes(), &parsed) {
            Ok(()) => true,
            Err(argon2::password_hash::Error::Password) => false,
            Err(err) => return Err(format!("invalid argon2 hash: {}", err)),
        }
    } else if hash.starts_with("$2") {
        bcrypt::verify(password, hash).map_err(|err| format!("invalid bcrypt hash: {}", err))?
    } else {
        return Err("hash is not an argon2 or bcrypt hash".to_string());
    };
    Ok(vec![Value::bool(matches)])
}

// ---------------------------------------------------------------------------
// Authenticated symmetric encryption
// ---------------------------------------------------------------------------

/// Runs `$body` with `$A` bound to the AEAD cipher named by `$algorithm`.
macro_rules! with_cipher {
    ($algorithm:expr, $A:ident => $body:expr) => {
        match $algorithm {
            "aes-256-gcm" => {
                type $A = aes_gcm::Aes256Gcm;
                $body
            }
            "aes-128-gcm" => {
                type $A = aes_gcm::Aes128Gcm;
                $body
            }
            "chacha20-poly1305" => {
                type $A = chacha20poly1305::ChaCha20Poly1305;
                $body
            }
            "xchacha20-poly1305" => {
                type $A = chacha20poly1305::XChaCha20Poly1305;
                $body
            }
            other => Err(format!("unknown cipher, '{}'", other)),
        }
    };
}

fn cipher_key<A: KeyInit>(key: &[u8]) -> Result<A, String> {
    A::new_from_slice(key).map_err(|_| {
        format!(
            "key must be {} bytes for this cipher, got {}",
            A::key_size(),
            key.len()
        )
    })
}

/// Seals `plaintext` under a fresh random nonce; the output is
/// `nonce || ciphertext || tag` so that decryption needs only the key.
fn seal<A: Aead + KeyInit>(key: &[u8], plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = cipher_key::<A>(key)?;
    let mut nonce = Nonce::<A>::default();
    OsRng.fill_bytes(nonce.as_mut_slice());
    let sealed = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| "encryption failed".to_string())?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&sealed);
    Ok(out)
}

fn open<A: Aead + KeyInit>(key: &[u8], data: &[u8], aad: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = cipher_key::<A>(key)?;
    let nonce_len = <A as AeadCore>::NonceSize::to_usize();
    let tag_len = <A as AeadCore>::TagSize::to_usize();
    if data.len() < nonce_len + tag_len {
        return Err("ciphertext is too short".to_string());
    }
    let (nonce, sealed) = data.split_at(nonce_len);
    cipher
        .decrypt(Nonce::<A>::from_slice(nonce), Payload { msg: sealed, aad })
        .map_err(|_| "decryption failed: wrong key, corrupted data or mismatched aad".to_string())
}

fn builtin_generate_key(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let algorithm = str_arg(args, 0, "algorithm")?;
    let encoding = encoding_arg(args, 1)?;
    with_cipher!(algorithm, A => {
        let mut key = vec![0u8; A::key_size()];
        OsRng.fill_bytes(&mut key);
        Ok(string_value(encode(&key, encoding)))
    })
}

fn builtin_encrypt(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let algorithm = str_arg(args, 0, "algorithm")?;
    let key = str_arg(args, 1, "key")?;
    let plaintext = str_arg(args, 2, "s")?;
    let aad = optional_str_arg(args, 3, "aad")?;
    let encoding = encoding_arg(args, 4)?;
    let key = decode(key, encoding, "key")?;
    with_cipher!(algorithm, A => {
        let sealed = seal::<A>(&key, plaintext.as_bytes(), aad.as_bytes())?;
        Ok(string_value(encode(&sealed, encoding)))
    })
}

fn builtin_decrypt(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let algorithm = str_arg(args, 0, "algorithm")?;
    let key = str_arg(args, 1, "key")?;
    let ciphertext = str_arg(args, 2, "s")?;
    let aad = optional_str_arg(args, 3, "aad")?;
    let encoding = encoding_arg(args, 4)?;
    let key = decode(key, encoding, "key")?;
    let data = decode(ciphertext, encoding, "s")?;
    with_cipher!(algorithm, A => {
        let plaintext = open::<A>(&key, &data, aad.as_bytes())?;
        Ok(string_value(utf8(plaintext, "decrypted data")?))
    })
}

// ---------------------------------------------------------------------------
// Asymmetric keys
// ---------------------------------------------------------------------------

const RSA_OID: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.1");
const ED25519_OID: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.101.112");
const X25519_OID: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.101.110");

enum PrivateKey {
    Rsa(RsaPrivateKey),
    Ed25519(SigningKey),
    X25519(StaticSecret),
}

enum PublicKey {
    Rsa(RsaPublicKey),
    Ed25519(VerifyingKey),
    X25519(x25519_dalek::PublicKey),
}

impl PrivateKey {
    fn kind(&self) -> &'static str {
        match self {
            PrivateKey::Rsa(_) => "rsa",
            PrivateKey::Ed25519(_) => "ed25519",
            PrivateKey::X25519(_) => "x25519",
        }
    }

    fn public_key(&self) -> PublicKey {
        match self {
            PrivateKey::Rsa(key) => PublicKey::Rsa(key.to_public_key()),
            PrivateKey::Ed25519(key) => PublicKey::Ed25519(key.verifying_key()),
            PrivateKey::X25519(key) => PublicKey::X25519(x25519_dalek::PublicKey::from(key)),
        }
    }

    fn to_pem(&self) -> Result<String, String> {
        let pem = match self {
            PrivateKey::Rsa(key) => key.to_pkcs8_pem(LineEnding::LF).map(|pem| pem.to_string()),
            PrivateKey::Ed25519(key) => key.to_pkcs8_pem(LineEnding::LF).map(|pem| pem.to_string()),
            // x25519-dalek has no PKCS#8 support, but the document is just
            // the raw scalar in an OCTET STRING under the X25519 OID [RFC 8410].
            PrivateKey::X25519(key) => OctetStringRef::new(&key.to_bytes())
                .and_then(|scalar| scalar.to_der())
                .and_then(|scalar| {
                    PrivateKeyInfo::new(
                        AlgorithmIdentifierRef {
                            oid: X25519_OID,
                            parameters: None,
                        },
                        &scalar,
                    )
                    .to_pem(LineEnding::LF)
                })
                .map_err(pkcs8::Error::from),
        };
        pem.map_err(|err| format!("failed to encode private key: {}", err))
    }
}

impl PublicKey {
    fn kind(&self) -> &'static str {
        match self {
            PublicKey::Rsa(_) => "rsa",
            PublicKey::Ed25519(_) => "ed25519",
            PublicKey::X25519(_) => "x25519",
        }
    }

    fn to_pem(&self) -> Result<String, String> {
        let pem = match self {
            PublicKey::Rsa(key) => key.to_public_key_pem(LineEnding::LF),
            PublicKey::Ed25519(key) => key.to_public_key_pem(LineEnding::LF),
            PublicKey::X25519(key) => BitStringRef::from_bytes(key.as_bytes())
                .map_err(pkcs8::spki::Error::from)
                .and_then(|bits| {
                    SubjectPublicKeyInfoRef {
                        algorithm: AlgorithmIdentifierRef {
                            oid: X25519_OID,
                            parameters: None,
                        },
                        subject_public_key: bits,
                    }
                    .to_pem(LineEnding::LF)
                    .map_err(pkcs8::spki::Error::from)
                }),
        };
        pem.map_err(|err| format!("failed to encode public key: {}", err))
    }
}

fn parse_private_key(pem: &str) -> Result<PrivateKey, String> {
    if pem.contains("-----BEGIN RSA PRIVATE KEY-----") {
        return RsaPrivateKey::from_pkcs1_pem(pem)
            .map(PrivateKey::Rsa)
            .map_err(|err| format!("invalid RSA private key: {}", err));
    }
    let document = SecretDocument::from_pkcs8_pem(pem)
        .map_err(|err| format!("invalid private key: {}", err))?;
    let info: PrivateKeyInfo = document
        .decode_msg()
        .map_err(|err| format!("invalid private key: {}", err))?;
    match info.algorithm.oid {
        RSA_OID => RsaPrivateKey::try_from(info)
            .map(PrivateKey::Rsa)
            .map_err(|err| format!("invalid RSA private key: {}", err)),
        ED25519_OID => SigningKey::try_from(info)
            .map(PrivateKey::Ed25519)
            .map_err(|err| format!("invalid ed25519 private key: {}", err)),
        X25519_OID => {
            let scalar = OctetStringRef::from_der(info.private_key)
                .map_err(|err| format!("invalid x25519 private key: {}", err))?;
            let scalar: [u8; 32] = scalar
                .as_bytes()
                .try_into()
                .map_err(|_| "invalid x25519 private key: expected 32 bytes".to_string())?;
            Ok(PrivateKey::X25519(StaticSecret::from(scalar)))
        }
        oid => Err(format!("unsupported private key algorithm, {}", oid)),
    }
}

/// Accepts a public key, or a private key in its place (deriving the public
/// half) so that a verify call can be handed either.
fn parse_public_key(pem: &str) -> Result<PublicKey, String> {
    if pem.contains("PRIVATE KEY-----") {
        return parse_private_key(pem).map(|key| key.public_key());
    }
    if pem.contains("-----BEGIN RSA PUBLIC KEY-----") {
        return RsaPublicKey::from_pkcs1_pem(pem)
            .map(PublicKey::Rsa)
            .map_err(|err| format!("invalid RSA public key: {}", err));
    }
    let document =
        Document::from_public_key_pem(pem).map_err(|err| format!("invalid public key: {}", err))?;
    let info: SubjectPublicKeyInfoRef = document
        .decode_msg()
        .map_err(|err| format!("invalid public key: {}", err))?;
    match info.algorithm.oid {
        RSA_OID => RsaPublicKey::try_from(info)
            .map(PublicKey::Rsa)
            .map_err(|err| format!("invalid RSA public key: {}", err)),
        ED25519_OID => VerifyingKey::try_from(info)
            .map(PublicKey::Ed25519)
            .map_err(|err| format!("invalid ed25519 public key: {}", err)),
        X25519_OID => {
            let point: [u8; 32] = info
                .subject_public_key
                .as_bytes()
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or_else(|| "invalid x25519 public key: expected 32 bytes".to_string())?;
            Ok(PublicKey::X25519(x25519_dalek::PublicKey::from(point)))
        }
        oid => Err(format!("unsupported public key algorithm, {}", oid)),
    }
}

fn builtin_generate_keypair(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let algorithm = str_arg(args, 0, "algorithm")?;
    let bits = size_arg(args, 1, "bits")?;
    let key = match algorithm {
        "rsa" => {
            if !(1024..=8192).contains(&bits) {
                return Err("bits must be between 1024 and 8192".to_string());
            }
            RsaPrivateKey::new(&mut OsRng, bits)
                .map(PrivateKey::Rsa)
                .map_err(|err| format!("rsa key generation failed: {}", err))?
        }
        "ed25519" => PrivateKey::Ed25519(SigningKey::generate(&mut OsRng)),
        "x25519" => PrivateKey::X25519(StaticSecret::random_from_rng(OsRng)),
        other => return Err(format!("unknown key algorithm, '{}'", other)),
    };
    Ok(vec![
        Value::string(key.to_pem()?),
        Value::string(key.public_key().to_pem()?),
    ])
}

fn builtin_public_key(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let pem = str_arg(args, 0, "privateKey")?;
    Ok(string_value(parse_private_key(pem)?.public_key().to_pem()?))
}

// ---------------------------------------------------------------------------
// Signatures and public-key encryption
// ---------------------------------------------------------------------------

/// Runs `$body` with `$D` bound to a digest RSA padding schemes accept.
macro_rules! with_rsa_digest {
    ($hash:expr, $D:ident => $body:expr) => {
        match $hash {
            "sha1" => {
                type $D = sha1::Sha1;
                $body
            }
            "sha256" => {
                type $D = sha2::Sha256;
                $body
            }
            "sha384" => {
                type $D = sha2::Sha384;
                $body
            }
            "sha512" => {
                type $D = sha2::Sha512;
                $body
            }
            other => Err(format!(
                "unknown hash for rsa, '{}' (expected sha1, sha256, sha384 or sha512)",
                other
            )),
        }
    };
}

fn rsa_sign(
    key: RsaPrivateKey,
    message: &[u8],
    hash: &str,
    padding: &str,
) -> Result<Vec<u8>, String> {
    match padding {
        "pss" => with_rsa_digest!(hash, D => {
            let signer = rsa::pss::SigningKey::<D>::new(key);
            Ok(signer.sign_with_rng(&mut OsRng, message).to_vec())
        }),
        "pkcs1" => with_rsa_digest!(hash, D => {
            let signer = rsa::pkcs1v15::SigningKey::<D>::new(key);
            Ok(signer.sign(message).to_vec())
        }),
        other => Err(format!(
            "unknown rsa padding, '{}' (expected pss or pkcs1)",
            other
        )),
    }
}

fn rsa_verify(
    key: RsaPublicKey,
    message: &[u8],
    signature: &[u8],
    hash: &str,
    padding: &str,
) -> Result<bool, String> {
    match padding {
        "pss" => with_rsa_digest!(hash, D => {
            let verifier = rsa::pss::VerifyingKey::<D>::new(key);
            Ok(rsa::pss::Signature::try_from(signature)
                .is_ok_and(|signature| verifier.verify(message, &signature).is_ok()))
        }),
        "pkcs1" => with_rsa_digest!(hash, D => {
            let verifier = rsa::pkcs1v15::VerifyingKey::<D>::new(key);
            Ok(rsa::pkcs1v15::Signature::try_from(signature)
                .is_ok_and(|signature| verifier.verify(message, &signature).is_ok()))
        }),
        other => Err(format!(
            "unknown rsa padding, '{}' (expected pss or pkcs1)",
            other
        )),
    }
}

fn builtin_sign(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let pem = str_arg(args, 0, "privateKey")?;
    let message = str_arg(args, 1, "s")?;
    let hash = str_arg(args, 2, "hash")?;
    let padding = str_arg(args, 3, "padding")?;
    let encoding = encoding_arg(args, 4)?;
    let signature = match parse_private_key(pem)? {
        PrivateKey::Rsa(key) => rsa_sign(key, message.as_bytes(), hash, padding)?,
        PrivateKey::Ed25519(key) => key.sign(message.as_bytes()).to_vec(),
        key => {
            return Err(format!(
                "{} keys cannot sign (use rsa or ed25519)",
                key.kind()
            ));
        }
    };
    Ok(string_value(encode(&signature, encoding)))
}

fn builtin_verify(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let pem = str_arg(args, 0, "publicKey")?;
    let message = str_arg(args, 1, "s")?;
    let signature = str_arg(args, 2, "signature")?;
    let hash = str_arg(args, 3, "hash")?;
    let padding = str_arg(args, 4, "padding")?;
    let encoding = encoding_arg(args, 5)?;
    // A malformed signature is simply not a valid one.
    let Ok(signature) = decode(signature, encoding, "signature") else {
        return Ok(vec![Value::FALSE]);
    };
    let valid = match parse_public_key(pem)? {
        PublicKey::Rsa(key) => rsa_verify(key, message.as_bytes(), &signature, hash, padding)?,
        PublicKey::Ed25519(key) => ed25519_dalek::Signature::from_slice(&signature)
            .is_ok_and(|signature| key.verify_strict(message.as_bytes(), &signature).is_ok()),
        key => {
            return Err(format!(
                "{} keys cannot verify (use rsa or ed25519)",
                key.kind()
            ));
        }
    };
    Ok(vec![Value::bool(valid)])
}

fn builtin_public_encrypt(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let pem = str_arg(args, 0, "publicKey")?;
    let message = str_arg(args, 1, "s")?;
    let hash = str_arg(args, 2, "hash")?;
    let encoding = encoding_arg(args, 3)?;
    let PublicKey::Rsa(key) = parse_public_key(pem)? else {
        return Err("publicEncrypt needs an rsa key".to_string());
    };
    let sealed = with_rsa_digest!(hash, D => {
        key.encrypt(&mut OsRng, Oaep::new::<D>(), message.as_bytes())
            .map_err(|err| format!("rsa encryption failed: {}", err))
    })?;
    Ok(string_value(encode(&sealed, encoding)))
}

fn builtin_private_decrypt(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let pem = str_arg(args, 0, "privateKey")?;
    let ciphertext = str_arg(args, 1, "s")?;
    let hash = str_arg(args, 2, "hash")?;
    let encoding = encoding_arg(args, 3)?;
    let PrivateKey::Rsa(key) = parse_private_key(pem)? else {
        return Err("privateDecrypt needs an rsa key".to_string());
    };
    let data = decode(ciphertext, encoding, "s")?;
    let plaintext = with_rsa_digest!(hash, D => {
        key.decrypt(Oaep::new::<D>(), &data)
            .map_err(|_| "rsa decryption failed: wrong key or corrupted data".to_string())
    })?;
    Ok(string_value(utf8(plaintext, "decrypted data")?))
}

fn builtin_shared_secret(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let private = str_arg(args, 0, "privateKey")?;
    let public = str_arg(args, 1, "publicKey")?;
    let encoding = encoding_arg(args, 2)?;
    let PrivateKey::X25519(private) = parse_private_key(private)? else {
        return Err("sharedSecret needs x25519 keys".to_string());
    };
    let PublicKey::X25519(public) = parse_public_key(public)? else {
        return Err("sharedSecret needs x25519 keys".to_string());
    };
    let secret = private.diffie_hellman(&public);
    // A low-order public key yields an all-zero secret that the peer can
    // predict; refuse it rather than hand back a "shared" secret.
    if !secret.was_contributory() {
        return Err("invalid x25519 public key".to_string());
    }
    Ok(string_value(encode(secret.as_bytes(), encoding)))
}

// ---------------------------------------------------------------------------
// Randomness
// ---------------------------------------------------------------------------

fn builtin_random_bytes(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let length = length_arg(args, 0, "n")?;
    let encoding = encoding_arg(args, 1)?;
    let mut bytes = vec![0u8; length];
    OsRng.fill_bytes(&mut bytes);
    Ok(string_value(encode(&bytes, encoding)))
}

fn builtin_random_int(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let min = integer_arg(args, 0, "min")?;
    let max = integer_arg(args, 1, "max")?;
    if min > max {
        return Err("min must not be greater than max".to_string());
    }
    let value = OsRng.gen_range(min..=max);
    Ok(vec![Value::int(value)])
}

/// A random (version 4) UUID [RFC 9562].
fn builtin_uuid(_vm: &mut VM, _args: &[Value]) -> Result<Vec<Value>, String> {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = encode(&bytes, Encoding::Hex);
    Ok(string_value(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )))
}
