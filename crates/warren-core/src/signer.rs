//! Signed URL tokens: HMAC-SHA256 over "<instance_id>|<port>|<expires_at>",
//! encoded as base64url(payload).base64url(signature).

use std::sync::Arc;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use crate::{Clock, Error, Result, Signer};

type HmacSha256 = Hmac<Sha256>;

pub struct HmacSigner {
    key: Vec<u8>,
    clock: Arc<dyn Clock>,
}

impl HmacSigner {
    pub fn new(secret: &str, clock: Arc<dyn Clock>) -> Self {
        HmacSigner { key: secret.as_bytes().to_vec(), clock }
    }

    fn mac(&self, payload: &str) -> Vec<u8> {
        let mut m = HmacSha256::new_from_slice(&self.key).expect("hmac accepts any key length");
        m.update(payload.as_bytes());
        m.finalize().into_bytes().to_vec()
    }
}

impl Signer for HmacSigner {
    fn sign(&self, instance_id: &str, port: u16, expires_at: i64) -> String {
        let payload = format!("{instance_id}|{port}|{expires_at}");
        format!("{}.{}", URL_SAFE_NO_PAD.encode(payload.as_bytes()), URL_SAFE_NO_PAD.encode(self.mac(&payload)))
    }

    fn verify(&self, token: &str) -> Result<(String, u16)> {
        let malformed = || Error::forbidden("malformed token");
        let (p, s) = token.split_once('.').ok_or_else(malformed)?;
        let payload = String::from_utf8(URL_SAFE_NO_PAD.decode(p).map_err(|_| malformed())?).map_err(|_| malformed())?;
        let sig = URL_SAFE_NO_PAD.decode(s).map_err(|_| malformed())?;
        let mut m = HmacSha256::new_from_slice(&self.key).expect("hmac accepts any key length");
        m.update(payload.as_bytes());
        m.verify_slice(&sig).map_err(|_| Error::forbidden("invalid token signature"))?;
        let mut fields = payload.split('|');
        let id = fields.next().ok_or_else(malformed)?.to_string();
        let port: u16 = fields.next().ok_or_else(malformed)?.parse().map_err(|_| malformed())?;
        let exp: i64 = fields.next().ok_or_else(malformed)?.parse().map_err(|_| malformed())?;
        if fields.next().is_some() {
            return Err(malformed());
        }
        if self.clock.now_unix() > exp {
            return Err(Error::forbidden("token expired"));
        }
        Ok((id, port))
    }
}

/// Signed URL lifetime when the caller does not choose one.
pub const DEFAULT_TTL_SECS: i64 = 3600;

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(i64);
    impl Clock for Fixed {
        fn now_unix(&self) -> i64 {
            self.0
        }
    }

    #[test]
    fn round_trip() {
        let now = 1_800_000_000;
        let s = HmacSigner::new("secret", Arc::new(Fixed(now)));
        let tok = s.sign("ab12cd34ef", 7681, now + 3600);
        assert_eq!(s.verify(&tok).unwrap(), ("ab12cd34ef".to_string(), 7681));
        assert!(HmacSigner::new("other", Arc::new(Fixed(now))).verify(&tok).is_err());
        assert!(HmacSigner::new("secret", Arc::new(Fixed(now + 7200))).verify(&tok).is_err());
        assert!(s.verify(&format!("{tok}x")).is_err());
        assert!(s.verify("garbage").is_err());
    }
}
