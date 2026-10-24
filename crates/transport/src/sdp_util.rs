//! Minimal read-only SDP helpers. str0m owns SDP parsing for negotiation;
//! these only extract values the transport needs outside of str0m.

/// UDP ports of the `a=candidate` lines in an SDP blob, deduplicated.
pub fn candidate_ports(sdp: &str) -> Vec<u16> {
    let mut ports: Vec<u16> = sdp
        .lines()
        .filter_map(|l| l.trim().strip_prefix("a=candidate:"))
        .filter_map(|c| {
            // foundation component transport priority address port typ ...
            let parts: Vec<&str> = c.split_whitespace().collect();
            (parts.len() >= 6 && parts[2].eq_ignore_ascii_case("udp"))
                .then(|| parts[5].parse().ok())
                .flatten()
        })
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// Value of the first `a=mid:` line.
pub fn first_mid(sdp: &str) -> Option<String> {
    sdp.lines().find_map(|l| {
        l.trim()
            .strip_prefix("a=mid:")
            .map(|m| m.trim().to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SDP: &str = "v=0\r\n\
        m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
        a=mid:0\r\n\
        a=candidate:1 1 udp 2130706175 192.168.1.10 50000 typ host\r\n\
        a=candidate:2 1 udp 2130706175 127.0.0.1 50000 typ host\r\n\
        a=candidate:3 1 udp 2130706175 10.0.0.2 50001 typ host\r\n\
        a=candidate:4 1 tcp 2130706175 10.0.0.2 9 typ host\r\n";

    #[test]
    fn extracts_unique_udp_ports() {
        assert_eq!(candidate_ports(SDP), vec![50000, 50001]);
    }

    #[test]
    fn finds_mid() {
        assert_eq!(first_mid(SDP).as_deref(), Some("0"));
        assert_eq!(first_mid("v=0"), None);
    }
}
