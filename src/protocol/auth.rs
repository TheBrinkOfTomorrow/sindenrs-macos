//! The mutual SHA-256 challenge/response that gates streaming.
//!
//! Both secrets are fixed constants present in every driver build since V1.08; there is no
//! per-device key. Implemented here for interoperability so hardware the user owns works
//! with this driver.
//!
//! Leg 1 (host verifies gun): host sends command 170, then 32 nonce bytes; gun replies with
//! 32 bytes that must equal `SHA256(nonce || GUN_SECRET)`.
//!
//! Leg 2 (gun verifies host): host sends command 109; gun replies with a 32-byte challenge;
//! host sends `SHA256(challenge || HOST_SECRET)`; gun replies with the line `true`.

use sha2::{Digest, Sha256};

const GUN_SECRET: &[u8; 41] = b"SindenLightgun364294735243894HaveANiceDay";
const HOST_SECRET: &[u8; 32] = b"60341663085532170074617363215964";

/// What the gun must answer to our nonce.
pub fn expected_gun_response(nonce: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(nonce);
    h.update(GUN_SECRET);
    h.finalize().into()
}

/// What we answer to the gun's challenge.
pub fn host_response(challenge: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(challenge);
    h.update(HOST_SECRET);
    h.finalize().into()
}

/// A fresh nonce. The gun only requires 32 bytes; the stock driver hashes a GUID. We hash
/// wall-clock time, monotonic time, the pid and a counter, which is plenty for a value that
/// only needs to differ between sessions.
pub fn make_nonce() -> [u8; 32] {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = Sha256::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    h.update(now.as_nanos().to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    h.update(COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    let addr = &now as *const _ as usize; // ASLR adds a little entropy, harmlessly
    h.update(addr.to_le_bytes());
    h.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    // Known answers computed independently with `sha256sum` over the concatenations.
    #[test]
    fn known_answer_gun_response() {
        let nonce = [0u8; 32];
        assert_eq!(
            hex(&expected_gun_response(&nonce)),
            "bd974c0b63df85db34e5e5e74e719665a02a915f294b000a64525dc082dfc054"
        );
    }

    #[test]
    fn known_answer_host_response() {
        let challenge: [u8; 32] = core::array::from_fn(|i| u8::try_from(i).unwrap_or(0));
        assert_eq!(
            hex(&host_response(&challenge)),
            "97e26ae6b25bab57de9d06710576ec17d89606e34ea2c4b23aad33858f39f003"
        );
    }

    #[test]
    fn nonces_differ() {
        assert_ne!(make_nonce(), make_nonce());
    }
}
