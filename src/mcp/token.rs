//! `jykmcp_<id>_<secret>`: a 50-bit id and a 256-bit secret.
//!
//! The id is the record key (lowercase RFC 4648 base32, ten characters).
//! The secret is 64 lowercase hex digits held in a [`SecretString`].
//! [`McpToken`] has no `Display` and is not `serde::Serialize` or
//! `Deserialize`. Its `Debug` is `McpToken(<redacted>)`. The wire form
//! leaves this type only through [`McpToken::expose_wire`] and
//! [`McpToken::expose_secret`], which callers must not log.

use jykre_core::SecretString;
use secrecy::ExposeSecret as _;
use std::fmt;
use std::io;

/// Lowercase RFC 4648 base32, no padding: `a-z` and `2-7`.
const ID_ALPHABET: [u8; 32] = *b"abcdefghijklmnopqrstuvwxyz234567";
const ID_LEN: usize = 10;
/// Seven bytes is 56 bits. Encoding keeps the top 50 (ten groups of five).
const ID_RANDOM_BYTES: usize = 7;
const SECRET_BYTES: usize = 32;
const SECRET_HEX_LEN: usize = SECRET_BYTES * 2;
const PREFIX: &str = "jykmcp_";
const HEX: [u8; 16] = *b"0123456789abcdef";

/// The record key of an MCP token: ten characters from
/// `abcdefghijklmnopqrstuvwxyz234567`.
///
/// Not a secret. `Debug` and `Display` show the ten characters. [`McpToken`]'s
/// `Debug` does not. This module never logs the id.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct McpTokenId([u8; ID_LEN]);

impl McpTokenId {
    /// `Some` when `id` is exactly ten characters of the base32 alphabet.
    /// Uppercase, `0`, `1`, `8` and `9` are rejected.
    pub fn parse(id: &str) -> Option<Self> {
        if id.len() != ID_LEN || !id.bytes().all(is_id_byte) {
            return None;
        }
        let mut bytes = [0_u8; ID_LEN];
        // `id.len()` is `ID_LEN`, so this copy stays inside the array.
        bytes.copy_from_slice(id.as_bytes());
        Some(Self(bytes))
    }

    /// The ten characters. [`Self::parse`] and token generation only store
    /// the ASCII base32 alphabet, so the bytes are UTF-8.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or_default()
    }
}

impl fmt::Display for McpTokenId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl fmt::Debug for McpTokenId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("McpTokenId")
            .field(&self.as_str())
            .finish()
    }
}

/// An MCP client token, `jykmcp_<id>_<secret>`.
///
/// The secret is a [`SecretString`]: it is wiped on drop, and [`Clone`]
/// copies it into another value that wipes itself too. This type does not
/// implement `serde::Serialize` or `serde::Deserialize` (no derive and no
/// impl). The wire form is not configuration and must not be written to
/// `config.db` as JSON. There is no `Display`; printing goes through the
/// redacted `Debug` below.
#[derive(Clone)]
pub struct McpToken {
    id: McpTokenId,
    secret: SecretString,
}

impl fmt::Debug for McpToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("McpToken(<redacted>)")
    }
}

impl McpToken {
    /// Draws the id and the secret from the operating system's secure
    /// random source, through the `ring` provider rustls already uses
    /// (the same source as `jjp`'s token).
    ///
    /// The error is only "the source could not fill the buffer". It carries
    /// no token bytes.
    pub fn generate() -> io::Result<Self> {
        let mut id_bytes = [0_u8; ID_RANDOM_BYTES];
        fill_random(&mut id_bytes)?;
        let mut secret_bytes = [0_u8; SECRET_BYTES];
        fill_random(&mut secret_bytes)?;
        Ok(Self {
            id: encode_id(&id_bytes),
            secret: SecretString::from(encode_hex(&secret_bytes)),
        })
    }

    /// The full wire `jykmcp_<id>_<secret>`. Lowercase hex only. A query
    /// string, whitespace, a bad alphabet or the wrong lengths is `None`.
    /// The secret is copied into a [`SecretString`].
    pub fn parse(wire: &str) -> Option<Self> {
        let (id, secret) = split_wire(wire)?;
        Some(Self {
            id: McpTokenId::parse(id)?,
            secret: SecretString::from(secret),
        })
    }

    /// The record key. Not the secret.
    pub fn id(&self) -> &McpTokenId {
        &self.id
    }

    /// `jykmcp_<id>_<secret>` for the one-time display and the vault write.
    ///
    /// The caller must not log this string.
    pub fn expose_wire(&self) -> String {
        let mut wire = String::with_capacity(PREFIX.len() + ID_LEN + 1 + SECRET_HEX_LEN);
        wire.push_str(PREFIX);
        wire.push_str(self.id.as_str());
        wire.push('_');
        wire.push_str(self.expose_secret());
        wire
    }

    /// The 64 lowercase hex digits. The caller must not log them.
    pub fn expose_secret(&self) -> &str {
        self.secret.expose_secret()
    }

    /// Compares the secret part only, without stopping at the first
    /// differing byte when the lengths match.
    ///
    /// A different length is `false` before that loop, as `jjp`'s token
    /// does. Pass the secret slice from [`split_wire`], not the whole wire.
    pub fn secret_matches(&self, presented_secret: &str) -> bool {
        let expected = self.expose_secret().as_bytes();
        let presented = presented_secret.as_bytes();
        expected.len() == presented.len()
            && expected
                .iter()
                .zip(presented)
                .fold(0_u8, |difference, (left, right)| {
                    difference | (left ^ right)
                })
                == 0
    }
}

/// `(id, secret)` slices of a well-formed wire, with no allocation.
///
/// `None` when the prefix, lengths or alphabets are wrong, and when the
/// text contains whitespace or `?`. A bearer check looks the record up by
/// the id, then [`McpToken::secret_matches`] on the secret slice.
pub fn split_wire(wire: &str) -> Option<(&str, &str)> {
    if wire
        .bytes()
        .any(|byte| byte.is_ascii_whitespace() || byte == b'?')
    {
        return None;
    }
    let (id, secret) = wire.strip_prefix(PREFIX)?.split_once('_')?;
    if id.len() == ID_LEN
        && secret.len() == SECRET_HEX_LEN
        && id.bytes().all(is_id_byte)
        && secret.bytes().all(is_lower_hex)
    {
        Some((id, secret))
    } else {
        None
    }
}

/// `ring`'s secure random, the provider rustls already installs. Same call
/// `jjp`'s `Token::generate` makes.
fn fill_random(bytes: &mut [u8]) -> io::Result<()> {
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(bytes)
        .map_err(|_| io::Error::other("no secure random source"))
}

/// Top 50 bits of seven random bytes, as ten base32 characters.
fn encode_id(random: &[u8; ID_RANDOM_BYTES]) -> McpTokenId {
    let mut wide = 0_u64;
    for byte in random {
        wide = (wide << 8) | u64::from(*byte);
    }
    // Drop the low 6 of the 56 bits. What remains is ten groups of five,
    // most significant group first.
    let bits = wide >> 6;
    let mut id = [0_u8; ID_LEN];
    for (index, byte) in id.iter_mut().enumerate() {
        let shift = (ID_LEN - 1 - index) * 5;
        let group = ((bits >> shift) & 0b1_1111) as usize;
        // `group` is five bits, so it is an index into the 32-character alphabet.
        *byte = ID_ALPHABET[group];
    }
    McpTokenId(id)
}

/// 64 lowercase hex digits, the same text shape as `jjp`'s token.
fn encode_hex(bytes: &[u8; SECRET_BYTES]) -> String {
    let mut text = String::with_capacity(SECRET_HEX_LEN);
    for byte in bytes {
        // Each nibble is 0..=15, an index into `HEX`.
        text.push(char::from(HEX[usize::from(byte >> 4)]));
        text.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    text
}

fn is_id_byte(byte: u8) -> bool {
    ID_ALPHABET.contains(&byte)
}

fn is_lower_hex(byte: u8) -> bool {
    matches!(byte, b'0'..=b'9' | b'a'..=b'f')
}

#[cfg(test)]
mod tests {
    use super::*;

    // `McpToken` does not implement `serde::Serialize` or `Deserialize`.
    // That is the whole check: there is no derive and no impl. Do not add
    // either. The wire form is not JSON configuration.

    #[test]
    fn generate_produces_a_parseable_wire_and_distinct_tokens() {
        let first = McpToken::generate().unwrap();
        let mut second = McpToken::generate().unwrap();
        for _ in 0..4 {
            if first.expose_wire() != second.expose_wire() {
                break;
            }
            second = McpToken::generate().unwrap();
        }
        assert_ne!(first.expose_wire(), second.expose_wire());

        let wire = first.expose_wire();
        let (id, secret) = split_wire(&wire).unwrap();
        assert_eq!(id.len(), ID_LEN);
        assert_eq!(secret.len(), SECRET_HEX_LEN);
        assert_eq!(wire.len(), PREFIX.len() + ID_LEN + 1 + SECRET_HEX_LEN);
        assert_eq!(first.id().as_str(), id);
        assert_eq!(first.expose_secret(), secret);
        assert!(first.secret_matches(secret));

        let parsed = McpToken::parse(&wire).unwrap();
        assert_eq!(parsed.id(), first.id());
        assert_eq!(parsed.expose_wire(), wire);
        assert!(parsed.secret_matches(first.expose_secret()));
    }

    #[test]
    fn secret_matches_accepts_the_secret_and_rejects_one_bit_wrong_length_and_uppercase() {
        let secret = "ab".repeat(32);
        let token = McpToken::parse(&format!("jykmcp_abcdefghij_{secret}")).unwrap();
        assert!(token.secret_matches(&secret));

        let mut flipped = secret.clone().into_bytes();
        let last = flipped.len() - 1;
        flipped[last] ^= 1;
        let flipped = String::from_utf8(flipped).unwrap();
        assert_eq!(
            secret.as_bytes()[last] ^ flipped.as_bytes()[last],
            1,
            "the change is one bit"
        );
        assert!(!token.secret_matches(&flipped));
        assert!(!token.secret_matches(&secret[..63]));
        assert!(!token.secret_matches(&format!("{secret}0")));
        assert!(!token.secret_matches(""));
        assert!(!token.secret_matches(&secret.to_ascii_uppercase()));
    }

    #[test]
    fn debug_of_mcp_token_omits_the_id_and_the_secret() {
        let id = "234567abcd";
        let secret = "0123456789abcdef".repeat(4);
        let token = McpToken::parse(&format!("jykmcp_{id}_{secret}")).unwrap();
        let rendered = format!("{token:?}");
        assert_eq!(rendered, "McpToken(<redacted>)");
        assert!(!rendered.contains(secret.as_str()));
        assert!(!rendered.contains("0123456789abcdef"));
        assert!(!rendered.contains(id));
        assert_eq!(token.id().as_str(), id);
        assert!(format!("{:?}", token.id()).contains(id));
    }

    #[test]
    fn split_wire_and_parse_reject_a_bad_shape() {
        let secret = "ab".repeat(32);
        let good = format!("jykmcp_abcdefghij_{secret}");
        assert_eq!(split_wire(&good), Some(("abcdefghij", secret.as_str())));
        assert!(McpToken::parse(&good).is_some());
        assert_eq!(
            McpTokenId::parse("abcdefghij").map(|id| id.as_str().to_owned()),
            Some("abcdefghij".to_owned())
        );
        assert!(McpTokenId::parse("ABCDEFGHIJ").is_none());
        assert!(McpTokenId::parse("abcdefghi0").is_none());

        let rejected = [
            format!("abcdefghij_{secret}"),
            format!("mcp_abcdefghij_{secret}"),
            format!("JYKMCP_abcdefghij_{secret}"),
            format!("jykmcp_abcdefghi_{secret}"),
            format!("jykmcp_abcdefghijk_{secret}"),
            format!("jykmcp_abcdefghi0_{secret}"),
            format!("jykmcp_abcdefgh18_{secret}"),
            format!("jykmcp_ABCDEFGHIJ_{secret}"),
            format!("jykmcp_abcdefghij_{}", &secret[..63]),
            format!("jykmcp_abcdefghij_{}", secret.replacen('a', "g", 1)),
            format!("jykmcp_abcdefghij_{}", secret.to_ascii_uppercase()),
            format!("{good}0"),
            format!("{good}?x=1"),
            format!(" {good}"),
            format!("{good} "),
            format!("jykmcp_abcd efghij_{secret}"),
            format!("{good}\n"),
            format!("jykmcp_abcdefghij_{secret}_"),
        ];
        for wire in rejected {
            assert!(split_wire(&wire).is_none(), "{wire}");
            assert!(McpToken::parse(&wire).is_none(), "{wire}");
        }
    }

    #[test]
    fn encode_id_maps_the_top_fifty_bits_onto_the_base32_alphabet() {
        // Index 0 is `a`, index 1 is `b`, index 31 is `7`.
        assert_eq!(encode_id(&[0; 7]).as_str(), "aaaaaaaaaa");
        assert_eq!(encode_id(&[0; 7]).to_string(), "aaaaaaaaaa");
        assert_eq!(encode_id(&[0xff; 7]).as_str(), "7777777777");
        assert_eq!(encode_id(&[0xf8, 0, 0, 0, 0, 0, 0]).as_str(), "7aaaaaaaaa");
        assert_eq!(encode_id(&[0x08, 0, 0, 0, 0, 0, 0]).as_str(), "baaaaaaaaa");
        // The low 6 bits of the seventh byte are outside the 50.
        assert_eq!(encode_id(&[0, 0, 0, 0, 0, 0, 0x3f]).as_str(), "aaaaaaaaaa");
        // Bit 6 of that byte is the last id bit (alphabet index 1, `b`).
        assert_eq!(encode_id(&[0, 0, 0, 0, 0, 0, 0x40]).as_str(), "aaaaaaaaab");
        assert_eq!(
            McpTokenId::parse(encode_id(&[0x08, 0, 0, 0, 0, 0, 0]).as_str()),
            Some(encode_id(&[0x08, 0, 0, 0, 0, 0, 0]))
        );
    }

    #[test]
    fn encode_hex_writes_lowercase_nibbles() {
        let mut bytes = [0_u8; SECRET_BYTES];
        bytes[0] = 0x0a;
        bytes[1] = 0xfb;
        bytes[SECRET_BYTES - 1] = 0x10;
        let hex = encode_hex(&bytes);
        assert_eq!(hex.len(), SECRET_HEX_LEN);
        assert_eq!(&hex[..4], "0afb");
        assert_eq!(&hex[SECRET_HEX_LEN - 2..], "10");
        assert!(hex[4..SECRET_HEX_LEN - 2].bytes().all(|byte| byte == b'0'));
    }
}
