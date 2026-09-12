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
//! Handlers for UART connections to/from nodes
use std::path::PathBuf;

use super::serial_handler::{Handler, SerialError};
use crate::hal::NodeId;
use crate::serial_service::serial_handler::HandlerState;
use tokio::sync::Mutex;
use tokio_serial::{DataBits, Parity, StopBits};
use tracing::{error, warn};

const DEFAULT_BAUD: u32 = 115_200;
const T113_RELIABLE_BAUD: u32 = 230_400;

/// Collection of [`crate::serial_service::serial_handler::Handler`]
#[derive(Debug)]
pub struct SerialConnections {
    handlers: [Mutex<Handler>; 4],
}

impl SerialConnections {
    pub fn new(bauds: [u32; 4]) -> Self {
        let paths = get_serial_devices();
        let handlers = std::array::from_fn(|i| {
            let baud = bauds[i];
            if baud > T113_RELIABLE_BAUD {
                warn!(
                    "UART baud {} for node {} exceeds {}; T113 may not be reliable",
                    baud,
                    i + 1,
                    T113_RELIABLE_BAUD
                );
            }

            let mut handler = Handler::new(
                i + 1,
                paths[i],
                baud,
                DataBits::Eight,
                Parity::None,
                StopBits::One,
            );

            if let Err(e) = handler.run() {
                error!("handler run error: {}", e);
            }

            Mutex::new(handler)
        });

        SerialConnections { handlers }
    }

    pub async fn get_state(&self) -> Vec<HandlerState> {
        let mut states = Vec::with_capacity(4);
        for handler in &self.handlers {
            states.push(handler.lock().await.get_state());
        }
        states
    }

    pub async fn handler(&self, node: NodeId) -> tokio::sync::MutexGuard<'_, Handler> {
        self.handlers[node as usize].lock().await
    }

    pub async fn set_baud_rate(&self, node: NodeId, baud: u32) -> Result<(), SerialError> {
        if baud > T113_RELIABLE_BAUD {
            warn!(
                "UART baud {} for {} exceeds {}; T113 may not be reliable",
                baud,
                node.api_name(),
                T113_RELIABLE_BAUD
            );
        }
        self.handlers[node as usize]
            .lock()
            .await
            .set_baud_rate(baud)
    }
}

impl Default for SerialConnections {
    fn default() -> Self {
        Self::new([DEFAULT_BAUD; 4])
    }
}

/// This is a quick and dirty way to detect which serial devices to load. At some point in time the
/// mapping of ttySx devices to the uart ports changed to align them numerically with the nodes
/// switched the numbering of
fn get_serial_devices() -> [&'static str; 4] {
    let device_type = PathBuf::from("/sys/class/tty/ttyS3/device/of_node/device_type");
    if matches!(std::fs::read_to_string(device_type), Ok(str) if str == "uart3") {
        ["/dev/ttyS2", "/dev/ttyS1", "/dev/ttyS4", "/dev/ttyS5"]
    } else {
        ["/dev/ttyS1", "/dev/ttyS2", "/dev/ttyS3", "/dev/ttyS4"]
    }
}
