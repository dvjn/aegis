use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use zeroize::Zeroize;

type HmacSha256 = Hmac<Sha256>;

pub(super) fn hmac_digest(key: &[u8], domain: &[u8], selector: &[u8], secret: &[u8]) -> [u8; 32] {
    let mut mac = <HmacSha256 as KeyInit>::new_from_slice(key).expect("HMAC accepts any key");
    mac.update(domain);
    mac.update(selector);
    mac.update(secret);
    mac.finalize().into_bytes().into()
}

pub struct Password(String);
impl Password {
    pub fn new(value: String) -> Result<Self, &'static str> {
        if value.is_empty() || value.len() > 1024 {
            return Err("password must be 1..=1024 bytes");
        }
        Ok(Self(value))
    }
    pub(crate) fn bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}
impl Drop for Password {
    fn drop(&mut self) {
        self.0.zeroize()
    }
}

pub struct SessionToken(pub(crate) String);
impl SessionToken {
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}
impl Drop for SessionToken {
    fn drop(&mut self) {
        self.0.zeroize()
    }
}

pub struct CsrfToken(pub(crate) String);
impl CsrfToken {
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}
impl Drop for CsrfToken {
    fn drop(&mut self) {
        self.0.zeroize()
    }
}
