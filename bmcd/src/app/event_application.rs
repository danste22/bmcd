// Copyright 2023 Turing Machines
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/License-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
use super::bmc_application::BmcApplication;
use std::sync::Arc;

#[cfg(target_os = "linux")]
pub fn run_event_listener(instance: Arc<BmcApplication>) -> anyhow::Result<()> {
    use crate::utils::EventListener;
    use anyhow::Context;
    use evdev::KeyCode;
    use std::time::Duration;
    use tokio::sync::oneshot;

    EventListener::new(
        (instance, Option::<oneshot::Sender<()>>::None),
        "/dev/input/event0",
    )
    .add_action(KeyCode::KEY_1, 1, |(app, s)| {
        let (sender, receiver) = oneshot::channel();
        *s = Some(sender);

        let bmc = app.clone();
        tokio::spawn(async move {
            let long_press = tokio::time::timeout(Duration::from_secs(3), receiver)
                .await
                .is_err();
            if let Err(e) = bmc.toggle_power_states(long_press).await {
                tracing::error!("toggle power from button: {:#}", e);
            }
        });
    })
    .add_action(KeyCode::KEY_1, 0, |(_, sender)| {
        let _ = sender.take().and_then(|s| s.send(()).ok());
    })
    .add_action(KeyCode::KEY_POWER, 1, move |(app, _)| {
        let bmc = app.clone();
        tokio::spawn(async move {
            if let Err(e) = bmc.toggle_power_states(false).await {
                tracing::error!("toggle power from power key: {:#}", e);
            }
        });
    })
    .add_action(KeyCode::KEY_RESTART, 1, |(app, _)| {
        let bmc = app.clone();
        tokio::spawn(async move {
            if let Err(e) = bmc.reboot(false).await {
                tracing::error!("reboot from restart key: {:#}", e);
            }
        });
    })
    .run()
    .context("event_listener error")
}

#[cfg(not(target_os = "linux"))]
pub fn run_event_listener(_instance: Arc<BmcApplication>) -> anyhow::Result<()> {
    tracing::warn!("physical button listener is Linux-only");
    Ok(())
}
