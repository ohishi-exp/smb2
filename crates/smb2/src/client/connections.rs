//! Every connection an [`SmbClient`](super::SmbClient) holds, and the one way
//! to reach the right one for a [`Tree`].
//!
//! A `TreeId` only means something inside the session that issued it, and a
//! tree DFS moved to another server lives on that server's connection. Sent
//! over the wrong one, it either fails with `STATUS_NETWORK_NAME_DELETED` or,
//! when the ids collide, reads or writes a same-named file on a different
//! share. So the primary connection sits behind this type: a method holding a
//! `Tree` asks [`Connections::for_tree`], and reaching the primary directly
//! takes an explicit [`Connections::primary_mut`], which only calls that have
//! no tree (share listing, the namespace root's own tree connect, reconnect)
//! have any business with.

use std::collections::HashMap;

use log::debug;

use crate::client::connection::Connection;
use crate::client::session::Session;
use crate::client::tree::Tree;
use crate::error::Result;
use crate::Error;

/// A connection to a specific server with its authenticated session.
///
/// Used for DFS cross-server referrals where the client needs connections
/// to multiple servers simultaneously.
pub(crate) struct ConnectionEntry {
    /// The connection to the server.
    pub conn: Connection,
    /// The session as of the last authentication on this connection. Behind
    /// an `Arc` for the same reason the primary's is: a revival can establish
    /// a new one behind this client's back, and share encryption derives from
    /// the live keys.
    pub session: std::sync::Arc<Session>,
}

/// The primary connection plus the pool of extra connections for DFS targets,
/// keyed by `host:port`.
///
/// The pool never replaces the primary: that would invalidate every `Tree`
/// the caller already holds.
pub(crate) struct Connections {
    primary: Connection,
    /// `addr:port` of the primary, which is what a tree on it carries in
    /// [`Tree::server`].
    primary_server: String,
    extra: HashMap<String, ConnectionEntry>,
}

impl Connections {
    pub(crate) fn new(primary: Connection, primary_server: String) -> Self {
        Connections {
            primary,
            primary_server,
            extra: HashMap::new(),
        }
    }

    /// The connection `tree` lives on.
    ///
    /// ❌ **`Error::Disconnected`, never a panic.** A `Tree` outlives the pool
    /// entry it was resolved on: a reconnect drops every extra connection, and
    /// a consumer reasonably still holds the DFS-resolved `Tree` it had.
    /// `Disconnected` is both true and the classification whose documented
    /// response (`connect_share` again) is exactly right here.
    pub(crate) fn for_tree(&mut self, tree: &Tree) -> Result<&mut Connection> {
        if tree.server == self.primary_server {
            return Ok(&mut self.primary);
        }
        match self.extra.get_mut(&tree.server) {
            Some(entry) => Ok(&mut entry.conn),
            None => Err(orphaned(tree)),
        }
    }

    /// [`for_tree`](Self::for_tree) for a caller that only has `&self`, such
    /// as the handle openers, which clone the connection they get.
    pub(crate) fn for_tree_ref(&self, tree: &Tree) -> Result<&Connection> {
        if tree.server == self.primary_server {
            return Ok(&self.primary);
        }
        match self.extra.get(&tree.server) {
            Some(entry) => Ok(&entry.conn),
            None => Err(orphaned(tree)),
        }
    }

    /// The connection to `addr`, for a DFS referral that names a server
    /// rather than a tree.
    pub(crate) fn for_addr(&mut self, addr: &str) -> Result<&mut Connection> {
        if self.is_primary(addr) {
            return Ok(&mut self.primary);
        }
        self.extra
            .get_mut(addr)
            .map(|entry| &mut entry.conn)
            .ok_or(Error::Disconnected)
    }

    /// The primary connection, for the calls that have no tree to route by.
    pub(crate) fn primary(&self) -> &Connection {
        &self.primary
    }

    /// The primary connection, mutably, for the calls that have no tree to
    /// route by. ❌ Never for a call that has one: use
    /// [`for_tree`](Self::for_tree).
    pub(crate) fn primary_mut(&mut self) -> &mut Connection {
        &mut self.primary
    }

    /// `addr:port` of the primary connection.
    pub(crate) fn primary_server(&self) -> &str {
        &self.primary_server
    }

    pub(crate) fn is_primary(&self, addr: &str) -> bool {
        addr == self.primary_server
    }

    pub(crate) fn extra(&self, addr: &str) -> Option<&ConnectionEntry> {
        self.extra.get(addr)
    }

    pub(crate) fn extra_mut(&mut self, addr: &str) -> Option<&mut ConnectionEntry> {
        self.extra.get_mut(addr)
    }

    pub(crate) fn extras(&self) -> impl Iterator<Item = &ConnectionEntry> {
        self.extra.values()
    }

    pub(crate) fn insert_extra(&mut self, addr: String, entry: ConnectionEntry) {
        self.extra.insert(addr, entry);
    }

    /// Drop every DFS connection. Trees resolved on them get
    /// `Error::Disconnected` from here on.
    pub(crate) fn clear_extras(&mut self) {
        self.extra.clear();
    }
}

fn orphaned(tree: &Tree) -> Error {
    debug!(
        "smb_client: no connection for {} (share {:?}); the DFS target it was \
         resolved on is gone, so connect_share again",
        tree.server, tree.share_name
    );
    Error::Disconnected
}
