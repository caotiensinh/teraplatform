//! Encrypts what must never rest in plaintext: state, saved plans, sensitive
//! variable values. Reading refuses anything that is not sealed, so a row
//! written around the store cannot be slipped in as plaintext.

use crate::{Result, StoreError};
use tf_crypto::Crypto;

#[derive(Clone, Debug)]
pub struct Sealer(Crypto);

fn fail(what: &str, e: tf_crypto::CryptoError) -> StoreError {
    StoreError::Internal(format!("{what}: {e}"))
}

impl Sealer {
    pub fn new(c: Crypto) -> Self {
        Self(c)
    }

    pub fn seal_state(&self, ws: &str, content: &str) -> String {
        self.0.encrypt_str("state", ws, content)
    }

    pub fn open_state(&self, ws: &str, text: &str) -> Result<String> {
        self.0.decrypt_str("state", ws, text).map_err(|e| fail(&format!("state of workspace {ws}"), e))
    }

    pub fn seal_plan(&self, run: &str, blob: &[u8]) -> String {
        self.0.encrypt("plan", run, blob)
    }

    pub fn open_plan(&self, run: &str, text: &str) -> Result<Vec<u8>> {
        self.0.decrypt("plan", run, text).map_err(|e| fail(&format!("saved plan of run {run}"), e))
    }

    /// The webhook secret (`what` = "webhook") or the access token (`what` = "token") of a workspace's repository link.
    pub fn seal_vcs(&self, ws: &str, what: &str, value: &str) -> String {
        self.0.encrypt_str("vcs", &format!("{ws}|{what}"), value)
    }

    pub fn open_vcs(&self, ws: &str, what: &str, text: &str) -> Result<String> {
        self.0.decrypt_str("vcs", &format!("{ws}|{what}"), text).map_err(|e| fail(&format!("repository {what} of workspace {ws}"), e))
    }

    fn var_id(ws: &str, category: &str, key: &str) -> String {
        format!("{ws}|{category}|{key}")
    }

    pub fn seal_var(&self, ws: &str, category: &str, key: &str, value: &str) -> String {
        self.0.encrypt_str("variable", &Self::var_id(ws, category, key), value)
    }

    pub fn open_var(&self, ws: &str, category: &str, key: &str, text: &str) -> Result<String> {
        self.0
            .decrypt_str("variable", &Self::var_id(ws, category, key), text)
            .map_err(|e| fail(&format!("variable {key}"), e))
    }
}
