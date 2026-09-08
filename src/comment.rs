//! The launch comment — the only place a distributor's money is tied to content (`SPEC.md` §1.3).
//!
//! ```text
//! dig-rewards:v1:<store_id_hex>:<root_hex>
//! ```
//!
//! A writer emits lowercase; a reader accepts either case and compares **the 32 bytes the hex
//! denotes, never the text**. A comment that does not parse means "not a DIG rewards distributor" —
//! [`LaunchComment::parse`] returns `None`, which is a terminal non-error outcome, not a failure.
//! CHIP-0051 distributors legitimately exist for other purposes (§9.3), and reporting one as an
//! error would make every foreign distributor look like a broken DIG one.

use std::fmt;

use chia_protocol::Bytes32;

/// The literal prefix every DIG rewards launch comment carries, version included.
const COMMENT_PREFIX: &str = "dig-rewards:v1:";

/// How many hex characters encode one 32-byte value.
const HEX_CHARS_PER_BYTES32: usize = 64;

/// The generation a distributor's money is about: one `(store_id, root)` pair.
///
/// The two halves are carried together on purpose. `dig-mirror-coin/SPEC.md` §5.1 states them
/// together for the same reason: splitting them is an authorization difference between two
/// implementations, because holding a store at *some* root is not holding it at *this* root.
///
/// This value states which generation the distributor is about. It is **not** evidence that the
/// generation exists, is valid, or is held by anyone (§1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LaunchComment {
    /// The DIG store's launcher id.
    pub store_id: Bytes32,
    /// The exact generation root this distributor pays mirrors of.
    pub root: Bytes32,
}

impl LaunchComment {
    /// Name a generation, ready to be rendered into a launch comment.
    #[must_use]
    pub const fn new(store_id: Bytes32, root: Bytes32) -> Self {
        Self { store_id, root }
    }

    /// Read a launch comment.
    ///
    /// Returns `None` when `comment` is not a DIG rewards launch comment — a different prefix, a
    /// different version, the wrong number of fields, a half that is not exactly 64 hex characters,
    /// or a half that is not hex at all. Either case is accepted on read.
    ///
    /// `None` is a **classification, not an error**: it says "some other kind of distributor".
    #[must_use]
    pub fn parse(comment: &str) -> Option<Self> {
        let payload = comment.strip_prefix(COMMENT_PREFIX)?;
        let (store_id_hex, root_hex) = payload.split_once(':')?;

        Some(Self {
            store_id: parse_bytes32_hex(store_id_hex)?,
            root: parse_bytes32_hex(root_hex)?,
        })
    }
}

impl fmt::Display for LaunchComment {
    /// Render the canonical, lowercase form. A writer MUST emit this and nothing else.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{COMMENT_PREFIX}{}:{}",
            hex::encode(self.store_id),
            hex::encode(self.root)
        )
    }
}

/// Decode exactly 64 hex characters into a [`Bytes32`], in either case.
///
/// The length is checked before decoding so that a short half can never be zero-extended into a
/// different, valid-looking generation.
fn parse_bytes32_hex(text: &str) -> Option<Bytes32> {
    if text.len() != HEX_CHARS_PER_BYTES32 {
        return None;
    }

    let mut bytes = [0u8; 32];
    hex::decode_to_slice(text, &mut bytes).ok()?;
    Some(Bytes32::new(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> LaunchComment {
        LaunchComment::new(Bytes32::new([0xab; 32]), Bytes32::new([0x01; 32]))
    }

    #[test]
    fn launch_comment_round_trips_through_lowercase_text() {
        let rendered = sample().to_string();

        assert_eq!(
            rendered,
            "dig-rewards:v1:\
             abababababababababababababababababababababababababababababababab\
             :\
             0101010101010101010101010101010101010101010101010101010101010101"
        );
        assert_eq!(rendered, rendered.to_lowercase());
        assert_eq!(LaunchComment::parse(&rendered), Some(sample()));
    }

    #[test]
    fn launch_comment_accepts_uppercase_on_read_and_compares_bytes_not_text() {
        let lowercase = sample().to_string();
        let uppercase = format!(
            "{COMMENT_PREFIX}{}:{}",
            hex::encode_upper(sample().store_id),
            hex::encode_upper(sample().root)
        );

        assert_ne!(lowercase, uppercase, "the two texts differ");

        let from_upper = LaunchComment::parse(&uppercase).expect("uppercase hex is accepted");
        let from_lower = LaunchComment::parse(&lowercase).expect("lowercase hex is accepted");

        // The comparison that matters is over the 32 bytes, which are identical even though the
        // two source strings are not.
        assert_eq!(from_upper, from_lower);
        assert_eq!(from_upper.store_id.as_ref(), sample().store_id.as_ref());
    }

    #[test]
    fn launch_comment_rejects_a_wrong_prefix_as_a_non_error() {
        let hex64 = hex::encode([0xab; 32]);
        assert_eq!(
            LaunchComment::parse(&format!("dig-rewards:v2:{hex64}:{hex64}")),
            None
        );
        assert_eq!(
            LaunchComment::parse(&format!("dig-mirror:v1:{hex64}:{hex64}")),
            None
        );
        assert_eq!(LaunchComment::parse("Reward Distributor v1"), None);
        assert_eq!(LaunchComment::parse(""), None);
    }

    #[test]
    fn launch_comment_rejects_a_wrong_length_half() {
        let hex64 = hex::encode([0xab; 32]);
        let short = &hex64[..62];

        assert_eq!(
            LaunchComment::parse(&format!("{COMMENT_PREFIX}{short}:{hex64}")),
            None
        );
        assert_eq!(
            LaunchComment::parse(&format!("{COMMENT_PREFIX}{hex64}:{hex64}ab")),
            None
        );
        assert_eq!(
            LaunchComment::parse(&format!("{COMMENT_PREFIX}{hex64}")),
            None,
            "a missing root half names no generation"
        );
    }

    #[test]
    fn launch_comment_rejects_non_hex() {
        let hex64 = hex::encode([0xab; 32]);
        let not_hex = "z".repeat(64);

        assert_eq!(
            LaunchComment::parse(&format!("{COMMENT_PREFIX}{not_hex}:{hex64}")),
            None
        );
        assert_eq!(
            LaunchComment::parse(&format!("{COMMENT_PREFIX}{hex64}:{not_hex}")),
            None
        );
    }
}
