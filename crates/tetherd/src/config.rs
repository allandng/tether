use std::net::IpAddr;

use clap::Parser;

/// Tether host daemon: stream this machine's screen to an allowed controller
/// and inject the input events it sends back.
#[derive(Parser, Debug, Clone)]
#[command(name = "tetherd", version)]
pub struct Args {
    /// Interface address to bind. Must be loopback or a private/link-local
    /// LAN address; 0.0.0.0 and public addresses are refused.
    #[arg(long)]
    pub bind: IpAddr,

    /// TCP port to listen on.
    #[arg(long, default_value_t = 7878)]
    pub port: u16,

    /// Controller IP allowed to connect (repeat for multiple). Connections
    /// from any other address are dropped before the protocol handshake.
    #[arg(long = "allow", required = true)]
    pub allow: Vec<IpAddr>,
}

/// `tetherd` is a remote-control backdoor by design; refuse to listen on
/// anything that could be a public interface. Loopback is allowed for
/// same-machine development and tests.
pub fn validate_bind_addr(ip: IpAddr) -> Result<(), String> {
    let ok = match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local(),
    };
    if ip.is_unspecified() {
        return Err(format!(
            "refusing to bind {ip}: binding all interfaces would expose the daemon beyond the LAN"
        ));
    }
    if ok {
        Ok(())
    } else {
        Err(format!(
            "refusing to bind {ip}: not a loopback or private LAN address"
        ))
    }
}

pub fn ip_allowed(allow: &[IpAddr], peer: IpAddr) -> bool {
    allow.contains(&peer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn bind_validation_accepts_loopback_and_private() {
        for good in ["127.0.0.1", "10.0.0.5", "172.16.31.2", "192.168.1.50", "169.254.7.7", "::1"] {
            assert!(validate_bind_addr(ip(good)).is_ok(), "{good} should be bindable");
        }
    }

    #[test]
    fn bind_validation_rejects_unspecified_and_public() {
        for bad in ["0.0.0.0", "::", "8.8.8.8", "203.0.113.7", "2001:db8::1", "172.32.0.1"] {
            assert!(validate_bind_addr(ip(bad)).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn allowlist_is_exact_match() {
        let allow = vec![ip("192.168.1.20"), ip("127.0.0.1")];
        assert!(ip_allowed(&allow, ip("192.168.1.20")));
        assert!(ip_allowed(&allow, ip("127.0.0.1")));
        assert!(!ip_allowed(&allow, ip("192.168.1.21")));
        assert!(!ip_allowed(&allow, ip("10.0.0.1")));
    }
}
