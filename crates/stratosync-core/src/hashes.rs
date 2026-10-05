//! Content-hash normalization for ETag comparison.
//!
//! The same content hash reaches the DB in different spellings depending
//! on who produced it: rclone `lsjson --hash` gives lowercase hex
//! (`md5`, `sha1`, `quickxor`, …), Microsoft Graph gives `sha1Hash` as
//! uppercase hex and `quickXorHash` as standard base64, and rclone's own
//! `--base64` output uses the URL-safe alphabet. Comparing those strings
//! verbatim made every upload's `if_match` precondition fail.

use std::collections::HashMap;

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;

/// Normalize a content hash to lowercase hex. Accepts hex in any case,
/// or base64 in the standard or URL-safe alphabet, padded or not.
/// Returns `None` for strings that are neither (item IDs, garbage).
pub fn hash_to_hex(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if s.len() % 2 == 0 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(s.to_ascii_lowercase());
    }
    let bytes = [&STANDARD, &URL_SAFE, &STANDARD_NO_PAD, &URL_SAFE_NO_PAD]
        .iter()
        .find_map(|engine| engine.decode(s).ok())?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// True when `stored` names the same content as any hash the remote
/// reports. Item IDs and other non-hash values never match: they say
/// nothing about content, so trusting them would let an upload silently
/// overwrite a concurrent remote edit.
pub fn etag_matches_hashes(stored: &str, hashes: &HashMap<String, String>) -> bool {
    let Some(stored) = hash_to_hex(stored) else { return false };
    hashes.values()
        .filter_map(|h| hash_to_hex(h))
        .any(|h| h == stored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn gdrive_hashes() -> HashMap<String, String> {
        HashMap::from([
            ("md5".into(),    "8cdbe53eaf2dcf885824a5c1563e2a4a".into()),
            ("sha1".into(),   "847bfc9ae8a6ad75e03bbda5b0200194d2be6049".into()),
            ("sha256".into(), "d2195901579e9f48ab58e0faf82c523f5a39f9b895fa7a70e5ba1b801c11026e".into()),
        ])
    }

    /// Pair taken from `rclone hashsum quickxor [--base64]` on the same
    /// file; Graph's `quickXorHash` is the standard-alphabet spelling.
    const QX_HEX: &str = "53bf838a19d190e4c6f982d6d77e5ae1a718e984";
    const QX_B64_STD: &str = "U7+DihnRkOTG+YLW135a4acY6YQ=";
    const QX_B64_URL: &str = "U7-DihnRkOTG-YLW135a4acY6YQ=";

    #[test]
    fn hash_to_hex_normalizes_every_spelling() {
        assert_eq!(hash_to_hex("847BFC9AE8A6AD75E03BBDA5B0200194D2BE6049").as_deref(),
                   Some("847bfc9ae8a6ad75e03bbda5b0200194d2be6049"));
        assert_eq!(hash_to_hex(QX_B64_STD).as_deref(), Some(QX_HEX));
        assert_eq!(hash_to_hex(QX_B64_URL).as_deref(), Some(QX_HEX));
        assert_eq!(hash_to_hex(QX_HEX).as_deref(), Some(QX_HEX));
    }

    #[test]
    fn stored_md5_matches_remote_listing_that_prefers_sha1() {
        // gdrive delta stores md5Checksum; rclone stat's etag is sha1.
        assert!(etag_matches_hashes("8cdbe53eaf2dcf885824a5c1563e2a4a", &gdrive_hashes()));
        assert!(etag_matches_hashes("847bfc9ae8a6ad75e03bbda5b0200194d2be6049", &gdrive_hashes()));
        assert!(etag_matches_hashes("847BFC9AE8A6AD75E03BBDA5B0200194D2BE6049", &gdrive_hashes()));
    }

    #[test]
    fn graph_base64_quickxor_matches_rclone_hex_quickxor() {
        let remote = HashMap::from([("quickxor".to_string(), QX_HEX.to_string())]);
        assert!(etag_matches_hashes(QX_B64_STD, &remote));
        assert!(etag_matches_hashes(QX_HEX, &remote));
    }

    /// An item ID says nothing about content; matching on it would let an
    /// upload silently overwrite a concurrent remote edit.
    #[test]
    fn item_id_and_different_content_never_match() {
        assert!(!etag_matches_hashes("10TlMHh1Slezu3D1K0NaDI6HIJKVJLWhn", &gdrive_hashes()));
        assert!(!etag_matches_hashes("4139BC84D0721EB5#4139BC84D0721EB5!16481", &gdrive_hashes()));
        assert!(!etag_matches_hashes("00000000000000000000000000000000", &gdrive_hashes()));
        assert!(!etag_matches_hashes("8cdbe53eaf2dcf885824a5c1563e2a4a", &HashMap::new()));
    }
}
