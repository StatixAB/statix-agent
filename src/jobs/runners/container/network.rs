use std::{env, net::Ipv4Addr, process::Command as StdCommand};

pub(super) struct LxcBridgeNetwork {
    pub(super) gateway: Ipv4Addr,
    pub(super) prefix_len: u8,
}

pub(super) fn lxc_bridge_network() -> Option<LxcBridgeNetwork> {
    let bridge = env::var("STATIX_LXC_NETWORK_BRIDGE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "lxcbr0".to_string());
    let output = StdCommand::new("ip")
        .args(["-4", "-o", "addr", "show", "dev", &bridge])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    parse_lxc_bridge_network(&String::from_utf8_lossy(&output.stdout))
}

pub(super) fn parse_lxc_bridge_network(output: &str) -> Option<LxcBridgeNetwork> {
    output
        .split_whitespace()
        .find_map(|field| field.split_once('/'))
        .and_then(|(address, prefix)| {
            let gateway = address.parse::<Ipv4Addr>().ok()?;
            let prefix_len = prefix.parse::<u8>().ok()?;
            (prefix_len <= 30).then_some(LxcBridgeNetwork {
                gateway,
                prefix_len,
            })
        })
}

pub(super) fn guest_ipv4_address(network: &LxcBridgeNetwork, container_name: &str) -> Ipv4Addr {
    let mut octets = network.gateway.octets();
    let host_octet = 10
        + (container_name
            .bytes()
            .fold(0u16, |acc, byte| acc.wrapping_add(u16::from(byte)))
            % 240) as u8;
    if host_octet == octets[3] {
        octets[3] = host_octet.saturating_add(1);
    } else {
        octets[3] = host_octet;
    }
    Ipv4Addr::from(octets)
}

pub(super) fn guest_network_command(network: &LxcBridgeNetwork, guest_address: Ipv4Addr) -> String {
    format!(
        "if ip -4 route show default | grep -q .; then exit 0; fi; iface=$(ip -o link show | awk -F': ' '$2 != \"lo\" {{print $2; exit}}' | cut -d@ -f1); test -n \"$iface\"; ip link set \"$iface\" up; ip -4 addr show dev \"$iface\" | grep -q 'inet ' || ip addr add {guest_address}/{prefix_len} dev \"$iface\"; ip route replace default via {gateway} dev \"$iface\"",
        prefix_len = network.prefix_len,
        gateway = network.gateway
    )
}
