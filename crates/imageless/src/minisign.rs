//! Verification of minisign signatures, the format SPEC §6.1 adopts for
//! release-manifest sidecars. Only verification lives here: signing is the
//! publisher's job, done with the stock `minisign` tool, so the node's code
//! and the signer's never have to agree on anything but this format.
//!
//! A signature file is four lines:
//!
//! ```text
//! untrusted comment: <free text, not covered by any signature>
//! base64(algorithm[2] ‖ key id[8] ‖ signature[64])
//! trusted comment: <free text>
//! base64(global signature[64])
//! ```
//!
//! `algorithm` is `Ed` (the signature is over the message itself) or `ED`
//! (over its BLAKE2b-512 hash, minisign's default since 0.10). The global
//! signature covers the first signature followed by the trusted comment, so a
//! trusted comment cannot be swapped between signatures.

use base64::Engine as _;
use blake2::{Blake2b512, Digest as _};
use ed25519_dalek::{Signature, VerifyingKey};

/// Bounds the sidecar read. `minisign` writes about 300 bytes; the rest is
/// headroom for a long trusted comment.
pub(crate) const MAX_SIGNATURE_BYTES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PublicKey {
    key_id: [u8; 8],
    key: VerifyingKey,
}

impl PublicKey {
    /// Parses the base64 line of a minisign public key — what `minisign -P`
    /// takes and the second line of a `.pub` file holds.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let bytes = decode(text)?;
        if bytes.len() != 42 || &bytes[..2] != b"Ed" {
            return None;
        }
        let key_id = bytes[2..10].try_into().ok()?;
        let key = VerifyingKey::from_bytes(bytes[10..].try_into().ok()?).ok()?;
        Some(Self { key_id, key })
    }

    pub(crate) fn key_id(&self) -> [u8; 8] {
        self.key_id
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SignatureError {
    /// Not a minisign signature file.
    Malformed,
    /// Well formed, but made by a key the verifier was not given.
    UnknownKey,
    /// Made by a trusted key's id, but the signature does not verify.
    Invalid,
}

/// Verifies `sidecar` as a minisign signature of `message` by one of `keys`.
pub(crate) fn verify(
    sidecar: &[u8],
    message: &[u8],
    keys: &[PublicKey],
) -> Result<(), SignatureError> {
    let text = std::str::from_utf8(sidecar).map_err(|_| SignatureError::Malformed)?;
    let text = text.strip_suffix('\n').unwrap_or(text);
    let lines: Vec<&str> = text.split('\n').collect();
    let [untrusted, signature, trusted, global] = lines[..] else {
        return Err(SignatureError::Malformed);
    };
    if !untrusted.starts_with("untrusted comment: ") {
        return Err(SignatureError::Malformed);
    }
    let trusted = trusted
        .strip_prefix("trusted comment: ")
        .ok_or(SignatureError::Malformed)?;
    let signature = decode(signature).ok_or(SignatureError::Malformed)?;
    let global = decode(global).ok_or(SignatureError::Malformed)?;
    if signature.len() != 74 || global.len() != 64 {
        return Err(SignatureError::Malformed);
    }
    let prehashed = match &signature[..2] {
        b"Ed" => false,
        b"ED" => true,
        _ => return Err(SignatureError::Malformed),
    };
    let key = keys
        .iter()
        .find(|key| key.key_id[..] == signature[2..10])
        .ok_or(SignatureError::UnknownKey)?;
    let manifest_signature =
        Signature::from_slice(&signature[10..]).map_err(|_| SignatureError::Malformed)?;
    let verified = if prehashed {
        key.key
            .verify_strict(&Blake2b512::digest(message), &manifest_signature)
    } else {
        key.key.verify_strict(message, &manifest_signature)
    };
    verified.map_err(|_| SignatureError::Invalid)?;
    let mut covered = signature[10..].to_vec();
    covered.extend_from_slice(trusted.as_bytes());
    let global = Signature::from_slice(&global).map_err(|_| SignatureError::Malformed)?;
    key.key
        .verify_strict(&covered, &global)
        .map_err(|_| SignatureError::Invalid)
}

fn decode(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(text).ok()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ed25519_dalek::{Signer as _, SigningKey};

    /// Signs like `minisign -S`, for tests that need a signed catalog without
    /// the tool. The golden vector below pins this against real minisign.
    pub(crate) fn sign(
        secret: &SigningKey,
        key_id: [u8; 8],
        message: &[u8],
        trusted: &str,
    ) -> String {
        let engine = base64::engine::general_purpose::STANDARD;
        let signature = secret.sign(&Blake2b512::digest(message)).to_bytes();
        let mut line = b"ED".to_vec();
        line.extend_from_slice(&key_id);
        line.extend_from_slice(&signature);
        let mut covered = signature.to_vec();
        covered.extend_from_slice(trusted.as_bytes());
        let global = secret.sign(&covered).to_bytes();
        format!(
            "untrusted comment: test signature\n{}\ntrusted comment: {trusted}\n{}\n",
            engine.encode(line),
            engine.encode(global)
        )
    }

    pub(crate) fn public_key_line(secret: &SigningKey, key_id: [u8; 8]) -> String {
        let mut bytes = b"Ed".to_vec();
        bytes.extend_from_slice(&key_id);
        bytes.extend_from_slice(secret.verifying_key().as_bytes());
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    // Produced by minisign 0.12:
    //   minisign -G -W -p k.pub -s k.key
    //   printf 'imageless golden vector\n' > m && minisign -S -s k.key -m m -t 'golden'
    //   minisign -S -l -s k.key -m m -x m.legacy -t 'legacy'
    const GOLDEN_KEY: &str = include_str!("../testdata/minisign/key.pub");
    const GOLDEN_MESSAGE: &[u8] = include_bytes!("../testdata/minisign/message");
    const GOLDEN_SIGNATURE: &[u8] = include_bytes!("../testdata/minisign/message.minisig");
    const GOLDEN_LEGACY: &[u8] = include_bytes!("../testdata/minisign/message.legacy.minisig");

    fn golden_key() -> PublicKey {
        PublicKey::parse(GOLDEN_KEY.lines().nth(1).unwrap()).unwrap()
    }

    #[test]
    fn signatures_made_by_minisign_verify() {
        let keys = [golden_key()];
        assert_eq!(verify(GOLDEN_SIGNATURE, GOLDEN_MESSAGE, &keys), Ok(()));
        assert_eq!(verify(GOLDEN_LEGACY, GOLDEN_MESSAGE, &keys), Ok(()));
    }

    #[test]
    fn the_test_signer_matches_minisign() {
        let secret = SigningKey::from_bytes(&[7; 32]);
        let key = PublicKey::parse(&public_key_line(&secret, *b"testkey1")).unwrap();
        let sidecar = sign(&secret, *b"testkey1", b"manifest", "t");
        assert_eq!(verify(sidecar.as_bytes(), b"manifest", &[key]), Ok(()));
    }

    #[test]
    fn any_change_to_what_is_signed_fails() {
        let keys = [golden_key()];
        let mut message = GOLDEN_MESSAGE.to_vec();
        message[0] ^= 1;
        assert_eq!(
            verify(GOLDEN_SIGNATURE, &message, &keys),
            Err(SignatureError::Invalid)
        );

        // The trusted comment is covered by the global signature.
        let text = std::str::from_utf8(GOLDEN_SIGNATURE).unwrap();
        let forged = text.replace("trusted comment: golden", "trusted comment: golden!");
        assert_ne!(forged, text);
        assert_eq!(
            verify(forged.as_bytes(), GOLDEN_MESSAGE, &keys),
            Err(SignatureError::Invalid)
        );

        // The untrusted comment is, as its name says, not.
        let relabelled = text.replacen("untrusted comment: ", "untrusted comment: relabelled ", 1);
        assert_eq!(verify(relabelled.as_bytes(), GOLDEN_MESSAGE, &keys), Ok(()));
    }

    #[test]
    fn only_the_given_keys_are_trusted() {
        let other = SigningKey::from_bytes(&[9; 32]);
        let other_key = PublicKey::parse(&public_key_line(&other, *b"otherkey")).unwrap();
        assert_eq!(
            verify(GOLDEN_SIGNATURE, GOLDEN_MESSAGE, &[other_key]),
            Err(SignatureError::UnknownKey)
        );
        assert_eq!(
            verify(GOLDEN_SIGNATURE, GOLDEN_MESSAGE, &[]),
            Err(SignatureError::UnknownKey)
        );

        // A key that claims the golden key's id is still checked by value.
        let impostor = PublicKey::parse(&public_key_line(&other, golden_key().key_id())).unwrap();
        assert_eq!(
            verify(GOLDEN_SIGNATURE, GOLDEN_MESSAGE, &[impostor]),
            Err(SignatureError::Invalid)
        );
    }

    #[test]
    fn malformed_signatures_and_keys_are_refused() {
        let keys = [golden_key()];
        let text = std::str::from_utf8(GOLDEN_SIGNATURE).unwrap();
        for bad in [
            String::new(),
            text.replacen("untrusted comment: ", "comment: ", 1),
            text.replacen("trusted comment: ", "trusted: ", 1),
            format!("{text}extra\n"),
            text.replace('\n', "\r\n"),
            text.lines().take(3).collect::<Vec<_>>().join("\n"),
        ] {
            assert_eq!(
                verify(bad.as_bytes(), GOLDEN_MESSAGE, &keys),
                Err(SignatureError::Malformed),
                "{bad:?}"
            );
        }
        let key_line = GOLDEN_KEY.lines().nth(1).unwrap();
        assert!(PublicKey::parse(key_line).is_some());
        assert!(PublicKey::parse(&key_line[1..]).is_none());
        assert!(PublicKey::parse("").is_none());
        assert!(
            PublicKey::parse(GOLDEN_KEY).is_none(),
            "a whole .pub file is not a key line"
        );
    }
}
