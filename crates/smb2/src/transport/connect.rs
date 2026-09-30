//! How a connect is bounded and spread across addresses, and what each
//! address did when it failed.
//!
//! Its own module, apart from [`tcp`](super::tcp), because it is part of the
//! public config and error types on every build, including the socketless
//! wasm one, where `tcp` doesn't exist.

use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

/// How a connect attempt is bounded, and how it is spread across the addresses
/// a name resolves to.
///
/// The thing this exists to prevent: `TcpStream::connect` walks every address
/// `getaddrinfo` returns, one at a time, and a single deadline around it means
/// one address that blackholes SYNs eats the whole budget while the live ones
/// are never dialled. Every AD domain name, and plenty of NASes, resolve to
/// several addresses, and a DFS namespace root makes it worse by construction:
/// the name being dialled *is* a domain name.
///
/// So attempts are staggered instead, RFC 8305's shape without the full
/// algorithm: start the next address after [`attempt_delay`](Self::attempt_delay),
/// leave the earlier ones running, first connected socket wins, and every
/// address gets a real chance inside the caller's budget.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ConnectOptions {
    /// Budget for the whole attempt, name resolution included.
    pub timeout: Duration,
    /// How long to wait before starting the next address, with earlier
    /// attempts left running. Zero dials every address at once, which puts a
    /// SYN on every interface of every server for every connect — rarely what
    /// you want.
    pub attempt_delay: Duration,
    /// Cap on how many resolved addresses to try. Clamped to at least 1, so a
    /// zero cannot make connecting impossible.
    ///
    /// Eight covers a realistically-sized set of domain controllers across
    /// both address families, and at the default stagger the eighth attempt
    /// starts 1.75 s in — comfortably inside any sane budget. A name with more
    /// addresses than that is a load-balanced pool where the extras are
    /// interchangeable, so trying them all buys nothing and costs a SYN each.
    pub max_addresses: usize,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            // RFC 8305 § 5 recommends 250 ms, with 2 s as the maximum.
            attempt_delay: Duration::from_millis(250),
            max_addresses: 8,
        }
    }
}

impl ConnectOptions {
    /// The defaults with the whole-attempt budget replaced.
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            timeout,
            ..Self::default()
        }
    }
}

/// What happened on one address of a name that could not be connected.
///
/// Part of [`Error::ConnectFailed`](crate::Error::ConnectFailed), which is the difference between "the
/// connect timed out" and "these four addresses were tried, three refused and
/// one never answered".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectAttempt {
    /// The address that was dialled.
    pub addr: SocketAddr,
    /// Why it failed, as a typed kind so nothing downstream is tempted to
    /// match on a message. `None` means the attempt ran out of budget rather
    /// than failing outright — it may still have been on its way.
    pub error_kind: Option<std::io::ErrorKind>,
}

impl fmt::Display for ConnectAttempt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.error_kind {
            Some(kind) => write!(f, "{}: {kind}", self.addr),
            None => write!(f, "{}: no answer within the budget", self.addr),
        }
    }
}
