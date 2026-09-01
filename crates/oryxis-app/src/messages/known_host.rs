//! Known-hosts management (delete one / clear all, with confirms), wrapped by [`crate::messages::Message::KnownHost`]. Handled by `Oryxis::handle_known_hosts`.

#[derive(Debug, Clone)]
pub enum KnownHostMessage {
    /// Open the confirm dialog before deleting a single known host.
    /// The index is read in the same update the row was clicked in, so
    /// it is still the row the user pointed at.
    RequestDeleteKnownHost(usize),
    /// The confirmed delete, carrying the entry's id rather than its
    /// position. The dialog blocks input, not async updates, and
    /// `list_known_hosts` is `ORDER BY hostname`: accepting a host key
    /// anywhere else reloads the list with the new entry INSERTED, so a
    /// cached position would delete whichever neighbour slid into it.
    DeleteKnownHost(uuid::Uuid),
    /// Open the confirm dialog before clearing every known host.
    RequestClearAllKnownHosts,
    ClearAllKnownHosts,
}
