// Copyright 2026 Oleg Tsarev
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! `/etc/network/interfaces` as the boot twin of `switch.json`.
//!
//! On confirm (and on boot heal when the file drifts), the address stanza and
//! the switch topology are written as one file so `ifup -a` brings up bond0 /
//! VLANs before bmcd starts. Live apply still goes through
//! [`super::switch_applier::plan`]; this module only persists and heals the
//! file.
//!
//! Address and switch services both call into here under
//! [`with_interfaces_lock`], so one confirm cannot overwrite the other.

use crate::app::address_document::{
    hook_argument, parse, resolv_conf, AddressDocument, INTERFACE, MARKER,
};
use crate::app::switch_applier::{interface_name, plan, Command, BOND};
use crate::app::switch_document::{PortId, Preset, SecondUplink, SwitchDocument};
use crate::utils::write_durably;
use std::path::Path;
use std::sync::Mutex;

/// Entire-line comment carrying the switch document this file was rendered
/// from. ifupdown-ng treats a leading `#` as a comment; mid-line `#` must not
/// appear inside the JSON (compact serde avoids that).
pub const SWITCH_HEADER_PREFIX: &str = "# bmcd-switch: ";

/// Paths where ifupdown-ng may install its bond executor on this image.
const BOND_EXECUTOR_CANDIDATES: &[&str] = &[
    "/usr/libexec/ifupdown-ng/bond",
    "/usr/lib/ifupdown-ng/bond",
    "/lib/ifupdown-ng/bond",
];

static INTERFACES_LOCK: Mutex<()> = Mutex::new(());

/// Serialise every read-modify-write of `/etc/network/interfaces`.
pub fn with_interfaces_lock<R>(f: impl FnOnce() -> R) -> R {
    let _guard = INTERFACES_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f()
}

/// True when ifupdown-ng can bring up a `bond-slaves` stanza at boot.
pub fn bond_executor_present() -> bool {
    // Unit tests assert on the native stanza; the container has no executor.
    if cfg!(test) {
        return true;
    }
    BOND_EXECUTOR_CANDIDATES
        .iter()
        .any(|path| Path::new(path).is_file())
}

/// Members of `br0`, derived from the same port names [`interface_name`] owns.
pub fn bridge_ports(switch: &SwitchDocument) -> String {
    let mut names: Vec<&str> = [PortId::Node1, PortId::Node2, PortId::Node3, PortId::Node4]
        .into_iter()
        .filter_map(interface_name)
        .collect();
    if switch.second_uplink == SecondUplink::Lag {
        names.push(BOND);
    } else {
        names.extend(["ge0", "ge1"]);
    }
    names.join(" ")
}

/// Flat document with the same uplink shape as `wanted`, so `plan` emits VLAN /
/// filtering / STP only — bond create/destroy stays in the native stanzas.
pub fn boot_baseline(wanted: &SwitchDocument) -> SwitchDocument {
    Preset::Flat {
        second_uplink: wanted.second_uplink,
    }
    .expand()
}

/// Full `/etc/network/interfaces` for this address + switch pair.
pub fn render(address: &AddressDocument, switch: &SwitchDocument) -> String {
    let header = format!(
        "{SWITCH_HEADER_PREFIX}{}\n",
        serde_json::to_string(switch).expect("SwitchDocument is serializable")
    );
    let ports = bridge_ports(switch);
    let mut out = format!(
        "{MARKER}\n\
         {header}\
         # Edit through the API or the Network / Switch tabs; this file is replaced whole on\n\
         # every confirmed change.\n\
         \n\
         auto lo\n\
         iface lo inet loopback\n\
         \n"
    );

    let lag = switch.second_uplink == SecondUplink::Lag;
    if lag && bond_executor_present() {
        out.push_str(
            "auto bond0\n\
             iface bond0 inet manual\n\
             \x20 bond-slaves ge0 ge1\n\
             \x20 bond-mode 802.3ad\n\
             \x20 bond-miimon 100\n\
             \n",
        );
    }

    out.push_str(&format!("auto {INTERFACE}\n"));
    match address {
        AddressDocument::Dhcp => {
            out.push_str(&format!(
                "iface {INTERFACE} inet dhcp\n\
                 \x20 bridge-ports {ports}\n\
                 \x20 pre-up /etc/network/nfs_check\n\
                 \x20 wait-delay 15\n\
                 \x20 hostname $(hostname)\n"
            ));
        }
        AddressDocument::Static(s) => {
            out.push_str(&format!(
                "iface {INTERFACE} inet static\n\
                 \x20 address {}\n\
                 \x20 netmask {}\n",
                s.address,
                s.netmask()
            ));
            if let Some(gw) = s.gateway {
                out.push_str(&format!("  gateway {gw}\n"));
            }
            out.push_str(&format!(
                "  bridge-ports {ports}\n\
                 \x20 pre-up /etc/network/nfs_check\n\
                 \x20 wait-delay 15\n"
            ));
            if !s.dns.is_empty() || s.search.is_some() {
                let names: Vec<String> = s.dns.iter().map(ToString::to_string).collect();
                if !names.is_empty() {
                    out.push_str(&format!("  dns-nameservers {}\n", names.join(" ")));
                }
                if let Some(search) = &s.search {
                    out.push_str(&format!("  dns-search {search}\n"));
                }
                out.push_str(&format!(
                    "  up printf '{}' > /etc/resolv.conf\n",
                    hook_argument(&resolv_conf(&s.dns, s.search.as_deref()))
                ));
            }
        }
    }

    for command in plan(&boot_baseline(switch), switch) {
        out.push_str(&format!("  post-up {}\n", shell_join(&command)));
    }
    out
}

/// Read the `# bmcd-switch:` header, if this daemon wrote one.
pub fn switch_from_header(contents: &str) -> Option<SwitchDocument> {
    for line in contents.lines() {
        let Some(json) = line.trim().strip_prefix(SWITCH_HEADER_PREFIX) else {
            continue;
        };
        return SwitchDocument::from_json_str(json);
    }
    None
}

/// Why `/etc/network/interfaces` may not be rewritten for a switch confirm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandWrittenInterfaces;

/// Address document to keep when rewriting interfaces for a switch confirm.
///
/// Returns [`HandWrittenInterfaces`] for a file this daemon did not write and
/// that is more than the image's own DHCP default — callers must opt in and
/// keep a copy, the same way the address service does.
pub fn address_for_rewrite(contents: &str) -> Result<AddressDocument, HandWrittenInterfaces> {
    if contents.trim().is_empty() {
        return Ok(AddressDocument::Dhcp);
    }
    let parsed = parse(contents);
    if parsed.ours {
        return Ok(parsed.document.unwrap_or(AddressDocument::Dhcp));
    }
    if is_image_default(contents) {
        return Ok(AddressDocument::Dhcp);
    }
    Err(HandWrittenInterfaces)
}

/// The image's stock file, compared on significant lines only.
pub fn is_image_default(contents: &str) -> bool {
    significant_lines(contents) == significant_lines(&AddressDocument::Dhcp.render())
}

fn significant_lines(text: &str) -> Vec<&str> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

/// Write `interfaces` from address + switch. Returns whether the bytes changed.
pub fn write_interfaces(
    path: &Path,
    address: &AddressDocument,
    switch: &SwitchDocument,
) -> std::io::Result<bool> {
    let wanted = render(address, switch);
    let current = std::fs::read_to_string(path).unwrap_or_default();
    if current == wanted {
        return Ok(false);
    }
    write_durably(path, wanted.as_bytes())?;
    Ok(true)
}

fn shell_join(command: &Command) -> String {
    command
        .iter()
        .map(|part| {
            if part.is_empty()
                || part.contains(|c: char| c.is_whitespace() || "\"'\\$`".contains(c))
            {
                // post-up is not a shell script with mid-line comments; still
                // quote empties and metacharacters so ifupdown-ng passes argv
                // intact.
                format!("'{}'", part.replace('\'', "'\\''"))
            } else {
                part.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::switch_document::SecondUplink;

    #[test]
    fn flat_render_keeps_independent_bridge_ports_and_marker() {
        let text = render(&AddressDocument::Dhcp, &Preset::flat().expand());
        assert!(text.starts_with(MARKER));
        assert!(text.contains(SWITCH_HEADER_PREFIX));
        assert!(text.contains("node1 node2 node3 node4 ge0 ge1"));
        assert!(!text.contains("auto bond0"));
    }

    #[test]
    fn lag_render_adds_bond0_and_bridge_ports() {
        let switch = Preset::Flat {
            second_uplink: SecondUplink::Lag,
        }
        .expand();
        let text = render(&AddressDocument::Dhcp, &switch);
        assert!(text.contains("auto bond0"));
        assert!(text.contains("node1 node2 node3 node4 bond0"));
        assert!(text.contains("bond-mode 802.3ad"));
        assert!(
            !text.contains("link add bond0"),
            "bond comes from the stanza, not post-up"
        );
    }

    #[test]
    fn header_round_trips_the_switch_document() {
        let switch = Preset::Trunk {
            management_vid: 10,
            node_vid: 20,
            second_uplink: SecondUplink::Lag,
        }
        .expand();
        let text = render(&AddressDocument::Dhcp, &switch);
        let back = switch_from_header(&text).expect("header");
        assert_eq!(back, switch);
    }

    #[test]
    fn trunk_hooks_program_br0_self() {
        let switch = Preset::Trunk {
            management_vid: 10,
            node_vid: 20,
            second_uplink: SecondUplink::Off,
        }
        .expand();
        let text = render(&AddressDocument::Dhcp, &switch);
        assert!(
            text.contains("post-up bridge vlan add dev br0 vid 10 pvid untagged self"),
            "{text}"
        );
    }

    #[test]
    fn hand_written_static_is_not_silently_rewritten_as_dhcp() {
        let hand = "auto br0\niface br0 inet static\n  address 10.1.2.3\n  netmask 255.255.0.0\n";
        assert_eq!(address_for_rewrite(hand), Err(HandWrittenInterfaces));
    }

    #[test]
    fn image_default_and_empty_rewrite_as_dhcp() {
        let image = "# interface file auto-generated by buildroot\n\nauto lo\niface lo inet loopback\n\nauto br0\niface br0 inet dhcp\n  bridge-ports node1 node2 node3 node4 ge0 ge1\n  pre-up /etc/network/nfs_check\n  wait-delay 15\n  hostname $(hostname)\n";
        assert_eq!(address_for_rewrite(image), Ok(AddressDocument::Dhcp));
        assert_eq!(address_for_rewrite(""), Ok(AddressDocument::Dhcp));
    }
}
