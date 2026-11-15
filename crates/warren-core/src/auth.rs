//! API-key authentication against the store.

use std::sync::Arc;

use async_trait::async_trait;

use crate::{ApiKey, Authenticator, Clock, Error, HEADER_API_KEY, Result, Store, Workspace, ids};

pub struct KeyAuthenticator {
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
}

impl KeyAuthenticator {
    pub fn new(store: Arc<dyn Store>, clock: Arc<dyn Clock>) -> Self {
        KeyAuthenticator { store, clock }
    }
}

#[async_trait]
impl Authenticator for KeyAuthenticator {
    async fn authenticate(&self, raw_key: &str) -> Result<(Workspace, ApiKey)> {
        let mut key = raw_key.trim();
        if key.len() >= 7 && key[..7].eq_ignore_ascii_case("bearer ") {
            key = key[7..].trim();
        }
        if key.is_empty() {
            return Err(Error::invalid_api_key(format!(
                "Missing API key. Send it as a Bearer token or in the {HEADER_API_KEY} header."
            )));
        }
        if !key.starts_with(ids::KEY_PREFIX) {
            return Err(Error::invalid_api_key("Invalid API key."));
        }
        let k = match self.store.get_api_key_by_hash(&ids::hash_key(key)).await {
            Ok(k) => k,
            Err(e) if e.is_not_found() => return Err(Error::invalid_api_key("Invalid API key.")),
            Err(e) => return Err(e),
        };
        let now = self.clock.now_unix();
        if k.expires_at != 0 && now > k.expires_at {
            return Err(Error::invalid_api_key("This API key has expired."));
        }
        let ws = self.store.get_workspace(&k.workspace_id).await?;
        if now - k.last_used_at > 60 {
            let _ = self.store.touch_api_key(&k.id, now).await;
        }
        Ok((ws, k))
    }
}
