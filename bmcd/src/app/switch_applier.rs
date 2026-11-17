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

//! Putting a document on the switch, and reading one back off it.
//!
//! The kernel's bridge is the interface to the switch: the DSA driver turns
//! `bridge vlan add` into register writes on the RTL8370MB-CG. So this module
//! builds `bridge` and `ip` command lines, and nothing here knows anything
//! about the hardware beyond the port names.
//!
//! ## Why the commands are built first and run second
//!
//! [`plan`] turns a document into the exact list of commands, and is a pure
//! function with tests. [`Applier::apply`] runs that list. Keeping them apart
//! means the thing most likely to be wrong -- the order and the flags -- can
//! be checked without a switch, and the thing that cannot be tested without
//! hardware is reduced to "run these, stop on the first failure".
//!
//! ## Order matters, and the order is not obvious
//!
//! Filtering goes on BEFORE the VLANs are written and off AFTER they are
//! cleared, because a bridge with filtering on and no VLANs configured
//! forwards nothing at all. Doing it the other way round takes the board off
//! the network in the middle of its own reconfiguration -- which the confirm
//! window would recover, but only after making an operator watch it happen.

use crate::app::switch_document::{PortConfig, PortId, SecondUplink, SwitchDocument};

/// The kernel's name for each front-panel / node port, as the device tree
/// spells it.
///
/// The BMC's own port has no netdev, so it is absent here. Its VLAN membership
/// is programmed on the bridge with the `self` flag — see [`vlan_target`].
pub fn interface_name(port: PortId) -> Option<&'static str> {
    match port {
        PortId::Node1 => Some("node1"),
        PortId::Node2 => Some("node2"),
        PortId::Node3 => Some("node3"),
        PortId::Node4 => Some("node4"),
        PortId::Ge0 => Some("ge0"),
        PortId::Ge1 => Some("ge1"),
        PortId::Bond0 => Some("bond0"),
        PortId::Bmc => None,
    }
}

/// Ports whose VLAN membership is programmed with `bridge vlan` for this
/// document: every physical port, plus `bond0` when it is in the table.
fn vlan_ports(document: &SwitchDocument) -> Vec<PortId> {
    let mut ports: Vec<PortId> = PortId::ALL.to_vec();
    if document.ports.contains_key(&PortId::Bond0) {
        ports.push(PortId::Bond0);
    }
    ports
}

pub const BOND: &str = "bond0";
pub const BOND_MODE: &str = "802.3ad";
pub const BOND_MIIMON: &str = "100";

/// The bridge every switch port is enslaved to.
pub const BRIDGE: &str = "br0";

/// Where a port's VLAN membership is written: netdev name, and whether the
/// command needs the bridge `self` flag.
///
/// Front-panel ports are their own devices. The BMC is the CPU port: there is
/// no `bmc` netdev, so membership is `bridge vlan … dev br0 … self`.
fn vlan_target(port: PortId) -> (&'static str, bool) {
    match interface_name(port) {
        Some(name) => (name, false),
        None => (BRIDGE, true),
    }
}

/// One command, as argv. A string would have to be parsed by a shell, and a
/// shell is not something this needs between it and the kernel.
pub type Command = Vec<String>;

fn cmd(parts: &[&str]) -> Command {
    parts.iter().map(|part| part.to_string()).collect()
}

fn vlan_del(dev: &str, vid: u16, bridge_self: bool) -> Command {
    // `self` last: that is how `bridge` marks the bridge's own VLAN entry.
    let mut out = vec![
        "bridge".into(),
        "vlan".into(),
        "del".into(),
        "dev".into(),
        dev.into(),
        "vid".into(),
        vid.to_string(),
    ];
    if bridge_self {
        out.push("self".into());
    }
    out
}

fn vlan_add(dev: &str, vid: u16, pvid_untagged: bool, bridge_self: bool) -> Command {
    let mut out = vec![
        "bridge".into(),
        "vlan".into(),
        "add".into(),
        "dev".into(),
        dev.into(),
        "vid".into(),
        vid.to_string(),
    ];
    if pvid_untagged {
        out.push("pvid".into());
        out.push("untagged".into());
    }
    if bridge_self {
        out.push("self".into());
    }
    out
}

/// Every command needed to move the switch from `running` to `wanted`.
///
/// Computed as a whole rather than emitted as the document is walked, because
/// the order is the part that matters and an order is not something you can
/// get right one line at a time.
pub fn plan(running: &SwitchDocument, wanted: &SwitchDocument) -> Vec<Command> {
    let mut commands = Vec::new();

    let turning_on = wanted.vlan_filtering && !running.vlan_filtering;
    let turning_off = !wanted.vlan_filtering && running.vlan_filtering;
    let was_lag = running.second_uplink == SecondUplink::Lag;
    let now_lag = wanted.second_uplink == SecondUplink::Lag;

    // ON FIRST. A bridge with filtering on and no VLANs yet forwards nothing,
    // so the VLANs have to exist before it starts caring about them... and
    // yet the kernel will not accept a VLAN on a bridge that is not filtering.
    // The resolution is that `bridge vlan add` works either way, and the
    // window between the two is the whole point of enabling filtering last.
    if turning_off {
        commands.push(cmd(&[
            "ip",
            "link",
            "set",
            "dev",
            BRIDGE,
            "type",
            "bridge",
            "vlan_filtering",
            "0",
        ]));
    }

    // VLAN dels always before adds. Bond create/destroy sits between them so
    // ge0/ge1 are not briefly both bridged and bonded with the wrong vids.
    // After a port joins the bridge the kernel gives it default_pvid 1; clear
    // that before the adds when the document does not want VID 1.
    plan_vlan_dels(&mut commands, running, wanted);
    if was_lag && !now_lag {
        commands.extend(teardown_lag());
        plan_kernel_pvid_clear(&mut commands, wanted, &[PortId::Ge0, PortId::Ge1]);
    } else if !was_lag && now_lag {
        commands.extend(setup_lag());
        plan_kernel_pvid_clear(&mut commands, wanted, &[PortId::Bond0]);
    }
    plan_vlan_adds(&mut commands, running, wanted, was_lag, now_lag);

    if turning_on {
        commands.push(cmd(&[
            "ip",
            "link",
            "set",
            "dev",
            BRIDGE,
            "type",
            "bridge",
            "vlan_filtering",
            "1",
        ]));
    }

    if wanted.stp != running.stp {
        commands.push(cmd(&[
            "ip",
            "link",
            "set",
            "dev",
            BRIDGE,
            "type",
            "bridge",
            "stp_state",
            if wanted.stp { "1" } else { "0" },
        ]));
    }

    commands
}

fn plan_vlan_dels(commands: &mut Vec<Command>, running: &SwitchDocument, wanted: &SwitchDocument) {
    let mut ports = vlan_ports(running);
    for port in vlan_ports(wanted) {
        if !ports.contains(&port) {
            ports.push(port);
        }
    }
    for port in ports {
        let (dev, bridge_self) = vlan_target(port);
        let was = running.ports.get(&port).cloned().unwrap_or_default();
        let now = wanted.ports.get(&port).cloned().unwrap_or_default();
        for vid in was.vlans().difference(&now.vlans()) {
            commands.push(vlan_del(dev, *vid, bridge_self));
        }
    }
}

/// Drop the kernel `default_pvid` left on a port that just joined `br0`.
fn plan_kernel_pvid_clear(commands: &mut Vec<Command>, wanted: &SwitchDocument, ports: &[PortId]) {
    for &port in ports {
        let now = wanted.ports.get(&port).cloned().unwrap_or_default();
        if now.vlans().contains(&1) {
            continue;
        }
        let (dev, bridge_self) = vlan_target(port);
        commands.push(vlan_del(dev, 1, bridge_self));
    }
}

fn plan_vlan_adds(
    commands: &mut Vec<Command>,
    running: &SwitchDocument,
    wanted: &SwitchDocument,
    was_lag: bool,
    now_lag: bool,
) {
    let mut ports = vlan_ports(wanted);
    for port in vlan_ports(running) {
        if !ports.contains(&port) {
            ports.push(port);
        }
    }
    for port in ports {
        let (dev, bridge_self) = vlan_target(port);
        let was = effective_was(port, running, was_lag, now_lag);
        let now = wanted.ports.get(&port).cloned().unwrap_or_default();

        for vid in &now.tagged {
            if was.tagged.contains(vid) {
                continue;
            }
            commands.push(vlan_add(dev, *vid, false, bridge_self));
        }
        if let Some(vid) = now.untagged {
            if was.untagged != Some(vid) {
                commands.push(vlan_add(dev, vid, true, bridge_self));
            }
        }
    }
}

/// Previous VLAN membership as the kernel will have it when add commands run.
///
/// After `setup_lag` / `teardown_lag`, a newly enslaved port carries
/// `default_pvid` 1 even if the prior document listed no VLANs for it.
fn effective_was(
    port: PortId,
    running: &SwitchDocument,
    was_lag: bool,
    now_lag: bool,
) -> PortConfig {
    let raw = running.ports.get(&port).cloned().unwrap_or_default();
    let just_joined_bridge = match port {
        PortId::Bond0 => !was_lag && now_lag,
        PortId::Ge0 | PortId::Ge1 => was_lag && !now_lag,
        _ => false,
    };
    if just_joined_bridge {
        PortConfig::untagged(1)
    } else {
        raw
    }
}

fn setup_lag() -> Vec<Command> {
    vec![
        cmd(&[
            "ip",
            "link",
            "add",
            BOND,
            "type",
            "bond",
            "mode",
            BOND_MODE,
            "miimon",
            BOND_MIIMON,
        ]),
        cmd(&["ip", "link", "set", "dev", "ge0", "nomaster"]),
        cmd(&["ip", "link", "set", "dev", "ge1", "nomaster"]),
        cmd(&["ip", "link", "set", "dev", "ge0", "down"]),
        cmd(&["ip", "link", "set", "dev", "ge1", "down"]),
        cmd(&["ip", "link", "set", "dev", "ge0", "master", BOND]),
        cmd(&["ip", "link", "set", "dev", "ge1", "master", BOND]),
        cmd(&["ip", "link", "set", "dev", "ge0", "up"]),
        cmd(&["ip", "link", "set", "dev", "ge1", "up"]),
        cmd(&["ip", "link", "set", "dev", BOND, "up"]),
        cmd(&["ip", "link", "set", "dev", BOND, "master", BRIDGE]),
    ]
}

fn teardown_lag() -> Vec<Command> {
    vec![
        cmd(&["ip", "link", "set", "dev", BOND, "nomaster"]),
        cmd(&["ip", "link", "set", "dev", "ge0", "nomaster"]),
        cmd(&["ip", "link", "set", "dev", "ge1", "nomaster"]),
        cmd(&["ip", "link", "set", "dev", BOND, "down"]),
        cmd(&["ip", "link", "delete", BOND]),
        cmd(&["ip", "link", "set", "dev", "ge0", "master", BRIDGE]),
        cmd(&["ip", "link", "set", "dev", "ge1", "master", BRIDGE]),
        cmd(&["ip", "link", "set", "dev", "ge0", "up"]),
        cmd(&["ip", "link", "set", "dev", "ge1", "up"]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::switch_document::{Preset, SecondUplink};

    fn trunk() -> SwitchDocument {
        Preset::Trunk {
            management_vid: 10,
            node_vid: 20,
            second_uplink: SecondUplink::Redundant,
        }
        .expand()
    }

    fn joined(commands: &[Command]) -> Vec<String> {
        commands.iter().map(|c| c.join(" ")).collect()
    }

    #[test]
    fn applying_a_document_to_itself_does_nothing() {
        assert!(plan(&trunk(), &trunk()).is_empty());
        assert!(plan(&Preset::flat().expand(), &Preset::flat().expand()).is_empty());
    }

    /// The BMC has no `bmc` netdev. Membership is `dev br0 … self`. Emitting
    /// `dev bmc` would fail at the first apply; omitting `self` leaves the
    /// BMC on VID 1 while Split/Trunk clear VID 1 from the front ports.
    #[test]
    fn no_command_ever_names_the_bmc_port() {
        let lines = joined(&plan(&Preset::flat().expand(), &trunk()));
        assert!(!lines.iter().any(|l| l.contains("dev bmc")), "{lines:#?}");
        assert_eq!(interface_name(PortId::Bmc), None);
    }

    #[test]
    fn the_bmc_vlan_is_programmed_on_br0_self() {
        let to_trunk = joined(&plan(&Preset::flat().expand(), &trunk()));
        assert!(
            to_trunk
                .iter()
                .any(|l| l == "bridge vlan del dev br0 vid 1 self"),
            "leave Flat's VID 1 on the bridge itself: {to_trunk:#?}"
        );
        assert!(
            to_trunk
                .iter()
                .any(|l| l == "bridge vlan add dev br0 vid 10 pvid untagged self"),
            "join the management VLAN on the bridge itself: {to_trunk:#?}"
        );

        let to_split = joined(&plan(&Preset::flat().expand(), &Preset::Split.expand()));
        assert!(
            to_split
                .iter()
                .any(|l| l.contains("dev br0") && l.ends_with(" self")),
            "Split must program br0 self: {to_split:#?}"
        );
    }

    /// A name is not a switch setting. Renaming a VLAN must not touch the
    /// hardware, or every relabelling would cost the board a reconfiguration
    /// of something that was already right.
    #[test]
    fn renaming_a_vlan_plans_nothing() {
        let running = trunk();
        let mut renamed = running.clone();
        renamed
            .names
            .insert(20, "the modules, out the back".to_string());
        assert_ne!(running, renamed, "the documents do differ");
        assert!(plan(&running, &renamed).is_empty());
    }

    /// Filtering must not be switched on before the VLANs exist, and must be
    /// switched off before they are removed. A bridge filtering with no VLANs
    /// forwards nothing.
    #[test]
    fn filtering_goes_on_last_and_comes_off_first() {
        let on = joined(&plan(&Preset::flat().expand(), &trunk()));
        let index = on
            .iter()
            .position(|l| l.contains("vlan_filtering 1"))
            .expect("filtering is turned on");
        assert_eq!(
            index,
            on.len() - 2,
            "filtering should be enabled after the VLANs: {on:#?}"
        );

        let off = joined(&plan(&trunk(), &Preset::flat().expand()));
        let index = off
            .iter()
            .position(|l| l.contains("vlan_filtering 0"))
            .expect("filtering is turned off");
        assert_eq!(
            index, 0,
            "filtering should be disabled before anything else: {off:#?}"
        );
    }

    /// A port moving between untagged VLANs must leave the old one before it
    /// joins the new one, or it is briefly in both.
    #[test]
    fn removals_come_before_additions() {
        let lines = joined(&plan(&Preset::Split.expand(), &trunk()));
        let first_add = lines.iter().position(|l| l.contains("vlan add")).unwrap();
        let last_del = lines.iter().rposition(|l| l.contains("vlan del")).unwrap();
        assert!(last_del < first_add, "{lines:#?}");
    }

    #[test]
    fn an_untagged_vlan_is_added_as_pvid_untagged() {
        let lines = joined(&plan(&Preset::flat().expand(), &trunk()));
        assert!(
            lines
                .iter()
                .any(|l| l == "bridge vlan add dev node1 vid 20 pvid untagged"),
            "{lines:#?}"
        );
    }

    #[test]
    fn a_tagged_vlan_is_added_without_pvid() {
        let lines = joined(&plan(&Preset::flat().expand(), &trunk()));
        assert!(
            lines.iter().any(|l| l == "bridge vlan add dev ge0 vid 10"),
            "{lines:#?}"
        );
        assert!(!lines
            .iter()
            .any(|l| l == "bridge vlan add dev ge0 vid 10 pvid untagged"));
    }

    #[test]
    fn spanning_tree_is_only_touched_when_it_changes() {
        let with_stp = joined(&plan(&Preset::flat().expand(), &trunk()));
        assert!(with_stp.iter().any(|l| l.contains("stp_state 1")));

        let no_change = joined(&plan(&trunk(), &trunk()));
        assert!(!no_change.iter().any(|l| l.contains("stp_state")));
    }

    /// Taking the second uplink out of service should touch ge1 and nothing
    /// else, which is the cheapest possible proof that the plan is a
    /// difference and not a rewrite.
    #[test]
    fn a_narrow_change_produces_a_narrow_plan() {
        let redundant = trunk();
        let off = Preset::Trunk {
            management_vid: 10,
            node_vid: 20,
            second_uplink: SecondUplink::Off,
        }
        .expand();
        let lines = joined(&plan(&redundant, &off));
        assert!(
            lines
                .iter()
                .all(|l| l.contains("ge1") || l.contains("stp_state")),
            "{lines:#?}"
        );
    }

    #[test]
    fn flat_to_flat_lag_creates_bond0_and_clears_ge_vlans() {
        let flat = Preset::flat().expand();
        let lag = Preset::Flat {
            second_uplink: SecondUplink::Lag,
        }
        .expand();
        let lines = joined(&plan(&flat, &lag));
        assert!(
            lines.iter().any(|l| l.contains("link add bond0 type bond")),
            "{lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l == "ip link set dev bond0 master br0"),
            "{lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("vlan del dev ge0") || l.contains("vlan del dev ge1")),
            "slaves leave the flat VLAN before bonding: {lines:#?}"
        );
    }

    #[test]
    fn lag_to_flat_tears_down_bond0() {
        let lag = Preset::Flat {
            second_uplink: SecondUplink::Lag,
        }
        .expand();
        let flat = Preset::flat().expand();
        let lines = joined(&plan(&lag, &flat));
        assert!(
            lines.iter().any(|l| l == "ip link delete bond0"),
            "{lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l == "ip link set dev ge0 master br0"),
            "{lines:#?}"
        );
    }

    #[test]
    fn flat_to_trunk_lag_clears_kernel_default_pvid_on_bond0() {
        let flat = Preset::flat().expand();
        let lag = Preset::Trunk {
            management_vid: 10,
            node_vid: 20,
            second_uplink: SecondUplink::Lag,
        }
        .expand();
        let lines = joined(&plan(&flat, &lag));
        assert!(
            lines.iter().any(|l| l == "bridge vlan del dev bond0 vid 1"),
            "bond0 joins br0 with default_pvid 1: {lines:#?}"
        );
    }

    #[test]
    fn lag_to_trunk_clears_kernel_default_pvid_on_ge_ports() {
        let lag = Preset::Flat {
            second_uplink: SecondUplink::Lag,
        }
        .expand();
        let trunk = trunk();
        let lines = joined(&plan(&lag, &trunk));
        assert!(
            lines.iter().any(|l| l == "bridge vlan del dev ge0 vid 1"),
            "ge0 rejoins br0 with default_pvid 1: {lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l == "bridge vlan del dev ge1 vid 1"),
            "ge1 rejoins br0 with default_pvid 1: {lines:#?}"
        );
    }
}
