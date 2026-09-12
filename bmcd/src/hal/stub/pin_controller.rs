// Copyright 2023 Turing Machines
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
use crate::hal::helpers::bit_iterator;
use crate::hal::NodeId;
use crate::hal::UsbMode;
use crate::hal::UsbRoute;
use std::fmt::Display;
use tracing::warn;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbArchitecture {
    UsbMux,
    UsbHub,
}

impl Display for UsbArchitecture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UsbArchitecture::UsbHub => f.write_str("Usb hub"),
            UsbArchitecture::UsbMux => f.write_str("Single bus"),
        }
    }
}

pub struct PinController {
    architecture: UsbArchitecture,
}

impl PinController {
    /// create a new Pin controller
    pub fn new(has_usb_switch: bool) -> anyhow::Result<Self> {
        Ok(PinController {
            architecture: if has_usb_switch {
                UsbArchitecture::UsbMux
            } else {
                UsbArchitecture::UsbHub
            },
        })
    }

    pub fn select_usb(&self, node: NodeId, mode: UsbMode) -> std::io::Result<()> {
        warn!("select USB for node {:?}, mode:{:?}", node, mode);
        Ok(())
    }

    pub fn set_usb_route(&self, route: UsbRoute) -> std::io::Result<()> {
        warn!("select USB route {:?}", route);
        Ok(())
    }

    pub fn set_usb_boot(&self, nodes_state: u8, nodes_mask: u8) -> std::io::Result<()> {
        let updates = bit_iterator(nodes_state, nodes_mask);

        for (idx, state) in updates {
            warn!(
                "updating usb_boot state of node {} to {}",
                idx + 1,
                if state != 0 { "enable" } else { "disable" }
            );
        }
        Ok(())
    }

    pub fn set_node1_usb_route(&self, alternative_port: bool) -> std::io::Result<()> {
        warn!("set node1 usb route alternative_port={}", alternative_port);
        Ok(())
    }

    pub fn usb_bus_type(&self) -> UsbArchitecture {
        self.architecture
    }
}

impl std::fmt::Debug for PinController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PinController")
    }
}
