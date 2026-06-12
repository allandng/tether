//! Transport-agnostic wire protocol for Tether.
//!
//! Message types and encode/decode land in Module 1; see docs/protocol.md.

/// Protocol version sent in the `Hello` exchange.
pub const PROTOCOL_VERSION: u16 = 1;

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_smoke() {
        assert_eq!(super::PROTOCOL_VERSION, 1);
    }
}
