//! Webhook signature verification and event routing.

use hmac::{Hmac, Mac};
use sha2::Sha256;

pub type HmacSha256 = Hmac<Sha256>;

/// Verify GitHub's `X-Hub-Signature-256` header (`sha256=<hex>`) against the body.
pub fn verify_signature(secret: &str, body: &[u8], signature_header: Option<&str>) -> bool {
    let Some(sig) = signature_header else {
        return false;
    };
    let Some(hex_part) = sig.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(expected) = hex::decode(hex_part) else {
        return false;
    };
    let Ok(mut mac) = <HmacSha256 as Mac>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signature_roundtrip() {
        let secret = "s3cr3t";
        let body = b"{\"hello\": \"world\"}";
        let mut mac = <HmacSha256 as Mac>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        assert!(verify_signature(secret, body, Some(&sig)));
        assert!(!verify_signature(secret, b"tampered", Some(&sig)));
        assert!(!verify_signature("wrong", body, Some(&sig)));
        assert!(!verify_signature(secret, body, None));
        assert!(!verify_signature(secret, body, Some("garbage")));
    }
}
