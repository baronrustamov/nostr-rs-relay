//! Client connection state
use crate::close::Close;
use crate::conn::Nip42AuthState::{AuthPubkey, Challenge, NoAuth};
use crate::error::Error;
use crate::error::Result;
use crate::event::Event;
use crate::subscription::Subscription;
use crate::utils::{host_str, unix_time};
use governor::clock::DefaultClock;
use governor::middleware::NoOpMiddleware;
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};
use nonzero_ext::nonzero;
use std::collections::HashMap;
use tracing::{debug, trace};
use uuid::Uuid;

/// A subscription identifier has a maximum length
const MAX_SUBSCRIPTION_ID_LEN: usize = 256;

/// NIP-42 authentication state
pub enum Nip42AuthState {
    /// The client is not authenticated yet
    NoAuth,
    /// The AUTH challenge sent
    Challenge(String),
    /// The client is authenticated
    AuthPubkey(String),
}

/// State for a client connection
pub struct ClientConn {
    /// Client IP (either from socket, or configured proxy header
    client_ip_addr: String,
    /// Unique client identifier generated at connection time
    client_id: Uuid,
    /// The current set of active client subscriptions
    subscriptions: HashMap<String, Subscription>,
    /// Per-connection maximum concurrent subscriptions
    max_subs: usize,
    /// Publisher rate limiter
    pub_limiter: Option<RateLimiter<NotKeyed, InMemoryState, DefaultClock, NoOpMiddleware>>,
    /// NIP-42 AUTH
    auth: Nip42AuthState,
}

impl Default for ClientConn {
    fn default() -> Self {
        Self::new("unknown".to_owned(), &Settings::default())
    }
}

impl ClientConn {
    /// Create a new, empty connection state.
    #[must_use]
    pub fn new(client_ip_addr: String, config: &Settings) -> Self {
        let client_id = Uuid::new_v4();
        ClientConn {
            client_ip_addr: client_ip_addr.clone(),
            client_id,
            subscriptions: HashMap::new(),
            max_subs: 32,
            pub_limiter: if config.limits.rate_limit_whitelist.contains(&client_ip_addr) {
                None
            } else {
                Some(RateLimiter::direct(Quota::per_minute(nonzero!(60_u32))))
            },
            auth: NoAuth,
        }
    }

    #[must_use]
    pub fn subscriptions(&self) -> &HashMap<String, Subscription> {
        &self.subscriptions
    }

    /// Check if the given subscription already exists
    #[must_use]
    pub fn has_subscription(&self, sub: &Subscription) -> bool {
        self.subscriptions.values().any(|x| x == sub)
    }

    /// Get a short prefix of the client's unique identifier, suitable
    /// for logging.
    #[must_use]
    pub fn get_client_prefix(&self) -> String {
        self.client_id.to_string().chars().take(8).collect()
    }

    #[must_use]
    pub fn ip(&self) -> &str {
        &self.client_ip_addr
    }

    #[must_use]
    pub fn auth_pubkey(&self) -> Option<&String> {
        match &self.auth {
            AuthPubkey(pubkey) => Some(pubkey),
            _ => None,
        }
    }

    #[must_use]
    pub fn auth_challenge(&self) -> Option<&String> {
        match &self.auth {
            Challenge(pubkey) => Some(pubkey),
            _ => None,
        }
    }

    /// Add a new subscription for this connection.
    /// # Errors
    ///
    /// Will return `Err` if the client has too many subscriptions, or
    /// if the provided name is excessively long.
    pub fn subscribe(&mut self, s: Subscription) -> Result<()> {
        let k = s.get_id();
        let sub_id_len = k.len();
        // prevent arbitrarily long subscription identifiers from
        // being used.
        if sub_id_len > MAX_SUBSCRIPTION_ID_LEN {
            debug!(
                "ignoring sub request with excessive length: ({})",
                sub_id_len
            );
            return Err(Error::SubIdMaxLengthError);
        }
        // check if an existing subscription exists, and replace if so
        if self.subscriptions.contains_key(&k) {
            self.subscriptions.remove(&k);
            self.subscriptions.insert(k, s.clone());
            trace!(
                "replaced existing subscription (cid: {}, sub: {:?})",
                self.get_client_prefix(),
                s.get_id()
            );
            return Ok(());
        }

        // check if there is room for another subscription.
        if self.subscriptions.len() >= self.max_subs {
            return Err(Error::SubMaxExceededError);
        }
        // add subscription
        self.subscriptions.insert(k, s);
        trace!(
            "registered new subscription, currently have {} active subs (cid: {})",
            self.subscriptions.len(),
            self.get_client_prefix(),
        );
        Ok(())
    }

    /// Remove the subscription for this connection.
    pub fn unsubscribe(&mut self, c: &Close) {
        // TODO: return notice if subscription did not exist.
        self.subscriptions.remove(&c.id);
        trace!(
            "removed subscription, currently have {} active subs (cid: {})",
            self.subscriptions.len(),
            self.get_client_prefix(),
        );
    }

    pub fn generate_auth_challenge(&mut self) {
        self.auth = Challenge(Uuid::new_v4().to_string());
    }

    pub fn authenticate(&mut self, event: &Event, relay_url: &String) -> Result<()> {
        match &self.auth {
            Challenge(_) => (),
            AuthPubkey(_) => {
                // already authenticated
                return Ok(())
            },
            NoAuth => {
                // unexpected AUTH request
                return Err(Error::AuthFailure);
            },
        }
        match event.validate() {
            Ok(_) => {
                if event.kind != 22242 {
                    return Err(Error::AuthFailure);
                }

                let curr_time = unix_time();
                let past_cutoff = curr_time - 600; // 10 minutes
                let future_cutoff = curr_time + 600; // 10 minutes
                if event.created_at < past_cutoff || event.created_at > future_cutoff {
                    return Err(Error::AuthFailure);
                }

                let mut challenge: Option<&String> = None;
                let mut relay: Option<&String> = None;

                for tag in &event.tags {
                    if tag.len() == 2 && tag.get(0) == Some(&"challenge".into()) {
                        challenge = tag.get(1);
                    }
                    if tag.len() == 2 && tag.get(0) == Some(&"relay".into()) {
                        relay = tag.get(1);
                    }
                }

                match (challenge, &self.auth) {
                    (Some(received_challenge), Challenge(sent_challenge)) => {
                        if received_challenge != sent_challenge {
                            return Err(Error::AuthFailure);
                        }
                    }
                    (_, _) => {
                        return Err(Error::AuthFailure);
                    }
                }

                match (relay.and_then(|url| host_str(url)), host_str(relay_url)) {
                    (Some(received_relay), Some(our_relay)) => {
                        if received_relay != our_relay {
                            return Err(Error::AuthFailure);
                        }
                    }
                    (_, _) => {
                        return Err(Error::AuthFailure);
                    }
                }

                self.auth = AuthPubkey(event.pubkey.clone());
                trace!(
                    "authenticated pubkey {} (cid: {})",
                    event.pubkey.chars().take(8).collect::<String>(),
                    self.get_client_prefix()
                );
                Ok(())
            }
            Err(_) => Err(Error::AuthFailure),
        }
    }

    /// False if over limit
    pub fn check_pub_rate_limit(&mut self) -> bool {
        if let Some(limiter) = &self.pub_limiter {
            limiter.check().is_ok()
        } else {
            true
        }
    }
}
