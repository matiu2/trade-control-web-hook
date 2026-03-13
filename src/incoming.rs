//! The yaml payload we'll be receiving from trading view
use serde::Deserialize;

/// The key of the authentication token secret
const AUTH_TOKEN: &str = "AUTH_TOKEN";

#[derive(Deserialize)]
pub struct Incoming {
    /// Authentication token
    token: String,
    /// Instrument short name to affect, eg EURUSD
    instrument: String,
    /// Action to perform
    action: Action,
}

#[derive(Deserialize)]
pub struct Authenticated {
    incoming: Incoming,
}

#[derive(Deserialize)]
pub enum Action {
    /// Close all open positions
    Close,
}

impl Incoming {
    /// Returns true if this payload is authentic
    pub fn authenticate(self, env: &worker::Env) -> Option<Authenticated> {
        let secret = crate::get_secret(AUTH_TOKEN, env)?;
        (secret == self.token).then_some(Authenticated { incoming: self })
    }
}

impl Authenticated {
    pub fn instrument(&self) -> &str {
        &self.incoming.instrument
    }
    pub fn action(&self) -> &Action {
        &self.incoming.action
    }
}
