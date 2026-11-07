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
//! Routes for legacy API present in versions <= 2.0.0 of the firmware.
use crate::api::into_legacy_response::LegacyResponse;
use crate::api::into_legacy_response::{LegacyResult, Null};
use crate::app::bmc_application::NodeInfo;
use crate::app::bmc_application::{BmcApplication, UsbConfig};
use crate::app::bmc_info::{
    get_fs_stat, get_ipv4_address, get_mac_address, get_net_interfaces, get_storage_info,
};
use crate::app::transfer_action::InitializeTransfer;
use crate::app::transfer_action::UpgradeCommand;
use crate::hal::{NodeId, UsbMode, UsbRoute};
use crate::serial_service::serial::SerialConnections;
use crate::serial_service::{legacy_serial_get_handler, legacy_serial_set_handler};
use crate::streaming_data_service::data_transfer::DataTransfer;
use crate::streaming_data_service::StreamingDataService;
use crate::utils::transfer_source::{validate_download_url, validate_local_transfer_path};
use actix_files::file_extension_to_mime;
use actix_multipart::Multipart;
use actix_web::guard::{fn_guard, GuardContext};
use actix_web::http::{header, StatusCode};
use actix_web::{get, post, web, HttpResponse, Responder};
use anyhow::Context;
use async_compression::tokio::bufread::GzipEncoder;
use async_compression::Level;
use board_info::{self, BoardInfoAttribute};
use bytes::Bytes;
use humansize::{format_size, DECIMAL};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::ffi::c_ulong;
use std::io::{self, Write};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use tokio_util::io::{ReaderStream, StreamReader};

use super::get_node_param;
type Query = web::Query<std::collections::HashMap<String, String>>;

const MAX_UPLOAD_CHUNK: usize = 1024 * 1024;

#[derive(Debug, Deserialize)]
struct TransferQuery {
    #[serde(rename = "type")]
    ty: Option<String>,
    file: Option<String>,
    local: Option<String>,
    sha256: Option<String>,
    length: Option<String>,
    skip_crc: Option<String>,
    node: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsbSetQuery {
    node: String,
    mode: String,
}

#[derive(Debug, Deserialize)]
struct PowerSetQuery {
    node1: Option<String>,
    node2: Option<String>,
    node3: Option<String>,
    node4: Option<String>,
}

/// version 1:
///
/// * get requests with type&opt queries
///
/// version 1.1:
///
/// * enabled HTTPS
/// * chunked upload of flash images
const API_VERSION: &str = "1.1";

pub fn config(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("")
            .route(
                web::get()
                    .guard(fn_guard(flash_status_guard))
                    .to(handle_flash_status),
            )
            .route(
                web::get()
                    .guard(fn_guard(flash_guard))
                    .to(handle_transfer_request),
            )
            .route(
                web::post()
                    .guard(fn_guard(set_node_info_guard))
                    .to(set_node_aux_info),
            )
            .route(web::get().to(api_entry)),
    )
    .service(handle_file_upload)
    .service(cancel_file_upload)
    .service(backup_handler);
}

pub fn info_config(cfg: &mut web::ServiceConfig) {
    cfg.service(info_handler);
}

fn flash_status_guard(context: &GuardContext<'_>) -> bool {
    let Some(query) = context.head().uri.query() else {
        return false;
    };
    query.contains("opt=get") && (query.contains("type=flash") || query.contains("type=firmware"))
}

fn flash_guard(context: &GuardContext<'_>) -> bool {
    let Some(query) = context.head().uri.query() else {
        return false;
    };
    query.contains("opt=set") && (query.contains("type=flash") || query.contains("type=firmware"))
}

fn parse_query_map<T: serde::de::DeserializeOwned>(query: &Query) -> LegacyResult<T> {
    serde_json::from_value(serde_json::to_value(&**query)?)
        .map_err(|e| LegacyResponse::bad_request(e.to_string()))
}

fn set_node_info_guard(context: &GuardContext<'_>) -> bool {
    let Some(query) = context.head().uri.query() else {
        return false;
    };
    query.contains("opt=set") && query.contains("type=node_info")
}

#[get("/backup")]
async fn backup_handler() -> impl Responder {
    let (tx, rx) = tokio::sync::mpsc::channel::<io::Result<Bytes>>(8);

    tokio::task::spawn_blocking(move || {
        struct ChannelWriter {
            tx: tokio::sync::mpsc::Sender<io::Result<Bytes>>,
        }

        impl Write for ChannelWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.tx
                    .blocking_send(Ok(Bytes::copy_from_slice(buf)))
                    .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))?;
                Ok(buf.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let writer = ChannelWriter { tx: tx.clone() };
        let mut builder = tar::Builder::new(writer);
        builder.mode(tar::HeaderMode::Deterministic);
        if let Err(e) = builder
            .append_dir_all(".", "/mnt/overlay/upper/")
            .and_then(|_| builder.finish())
        {
            let _ = tx.blocking_send(Err(e));
        }
    });

    let now = chrono::Local::now();
    let content_disposition = format!(
        r#"attachment; filename="tp2-backup-{}.tar.gz""#,
        now.format("%d-%m-%Y")
    );
    let reader = StreamReader::new(ReceiverStream::new(rx));
    let encoder = GzipEncoder::with_quality(reader, Level::Best);
    HttpResponse::Ok()
        .insert_header(header::ContentType(file_extension_to_mime("gz")))
        .insert_header((header::CONTENT_DISPOSITION, content_disposition))
        .streaming(ReaderStream::new(encoder))
}

#[get("/info")]
async fn info_handler() -> impl Responder {
    get_system_information().await.into()
}

async fn api_entry(
    bmc: web::Data<BmcApplication>,
    serial: web::Data<SerialConnections>,
    query: Query,
) -> impl Responder {
    let is_set = match query.get("opt").map(String::as_str) {
        Some("set") => true,
        Some("get") => false,
        _ => return LegacyResponse::bad_request("Missing `opt` parameter"),
    };

    let Some(ty) = query.get("type") else {
        return LegacyResponse::bad_request("Missing `type` parameter");
    };

    let bmc = bmc.as_ref();
    match (ty.as_ref(), is_set) {
        ("usb_boot", true) => usb_boot(bmc, query).await.into(),
        ("clear_usb_boot", true) => clear_usb_boot(bmc).into(),
        ("network", true) => reset_network(bmc).await.into(),
        ("nodeinfo", true) => set_node_info().into(),
        ("nodeinfo", false) => get_node_info(bmc).await.into(),
        ("node_info", false) => get_node_aux_info(bmc).await.into(),
        ("node_to_msd", true) => set_node_to_msd(bmc, query).await.into(),
        ("other", false) => get_system_information().await.into(),
        ("power", true) => set_node_power(bmc, query).await,
        ("power", false) => get_node_power(bmc).await.into(),
        ("reboot", true) => reboot(bmc, query).await.into(),
        ("reload", true) => reload_self().into(),
        ("reset", true) => reset_node(bmc, query).await.into(),
        ("sdcard", true) => format_sdcard().into(),
        ("sdcard", false) => get_sdcard_info(),
        ("uart", false) => legacy_serial_get_handler(serial, query).await.into(),
        ("uart", true) => legacy_serial_set_handler(serial, query).await.into(),
        ("usb", true) => set_usb_mode(bmc, query).await.into(),
        ("usb", false) => get_usb_mode(bmc).await.into(),
        ("usb_node1", true) => set_node1_usb_mode(bmc, query).await.into(),
        ("usb_node1", false) => get_node1_usb_mode(bmc).await.into(),
        ("info", false) => get_info().await.into(),
        ("cooling", false) => get_cooling_info().await.into(),
        ("cooling", true) => set_cooling_info(bmc, query).await.into(),
        ("about", false) => get_about().await.into(),
        _ => (
            StatusCode::BAD_REQUEST,
            format!("Invalid `type` parameter {}", ty),
        )
            .into(),
    }
}

#[allow(clippy::unused_unit)]
fn reload_self() -> impl Into<LegacyResponse> {
    tokio::task::spawn_blocking(move || {
        match Command::new("/etc/init.d/S94bmcd").arg("restart").status() {
            Ok(status) if status.success() => {}
            Ok(status) => tracing::error!("bmcd restart returned {status}"),
            Err(e) => tracing::error!("bmcd restart failed: {e}"),
        }
    });

    ()
}

struct OsReleaseFields {
    version: Option<String>,
    buildroot: Option<String>,
}

/// `version` is os-release `VERSION` (firmware). `buildroot` is `PRETTY_NAME`
/// with surrounding quotes removed. Missing keys stay unset.
fn os_release_fields(os_release: &HashMap<String, String>) -> OsReleaseFields {
    OsReleaseFields {
        version: os_release.get("VERSION").cloned(),
        buildroot: os_release
            .get("PRETTY_NAME")
            .map(|value| value.trim_matches('"').to_string()),
    }
}

fn about_body(
    fields: &OsReleaseFields,
    hostname: &str,
    board_model: &str,
    board_revision: &str,
    bmcd_version: &str,
    build_time: &str,
) -> serde_json::Value {
    json!(
        {
            "board_model": board_model,
            "board_revision": board_revision,
            "hostname": hostname,
            "api": API_VERSION,
            "version": fields.version.clone().unwrap_or_else(|| "unknown".to_string()),
            "bmcd_version": bmcd_version,
            "buildtime": build_time,
            "buildroot": fields.buildroot.clone().unwrap_or_else(|| "unknown".to_string()),
        }
    )
}

async fn get_about() -> impl Into<LegacyResponse> {
    let fields = match read_os_release().await {
        Ok(os_release) => os_release_fields(&os_release),
        Err(_) => OsReleaseFields {
            version: None,
            buildroot: None,
        },
    };
    let hostname = read_hostname().await.unwrap_or_default();
    let (board_model, board_revision) = read_board_model().await.unwrap_or_default();

    about_body(
        &fields,
        &hostname,
        &board_model,
        &board_revision,
        env!("CARGO_PKG_VERSION"),
        build_time::build_time_utc!("%Y-%m-%d %H:%M:%S-00:00"),
    )
}

async fn get_info() -> impl Into<LegacyResponse> {
    let storage = get_storage_info();
    let ips = get_net_interfaces().await;
    json!(
        {
            "ip": ips,
            "storage": storage,
        }
    )
}

async fn reboot(bmc: &BmcApplication, query: Query) -> LegacyResult<()> {
    bmc.reboot(query.contains_key("fel"))
        .await
        .map_err(Into::into)
}

async fn reset_node(bmc: &BmcApplication, query: Query) -> LegacyResult<()> {
    let node = get_node_param(&query)?;
    Ok(bmc.reset_node(node).await?)
}

async fn usb_boot(bmc: &BmcApplication, query: Query) -> LegacyResult<()> {
    let node = get_node_param(&query)?;
    bmc.usb_boot(node, true).await.map_err(Into::into)
}

fn clear_usb_boot(bmc: &BmcApplication) -> impl Into<LegacyResponse> {
    bmc.clear_usb_boot().context("clear USB boot mode")
}

async fn reset_network(bmc: &BmcApplication) -> impl Into<LegacyResponse> {
    bmc.rtl_reset().await.context("reset network switch")
}

fn set_node_info() -> impl Into<LegacyResponse> {
    // In previous versions of the firmware this was dead code
    LegacyResponse::not_implemented("Method type `set` on parameter `nodeinfo` is deprecated")
}

async fn get_node_info(bmc: &BmcApplication) -> impl Into<LegacyResponse> {
    match bmc.get_node_infos().await {
        Ok(infos) => json! {
           [{
                "node1": infos[0],
                "node2": infos[1],
                "node3": infos[2],
                "node4": infos[3],
           }]
        }
        .into(),
        Err(e) => LegacyResponse::from(e),
    }
}
async fn set_node_aux_info(
    bmc: web::Data<BmcApplication>,
    serial: web::Data<SerialConnections>,
    payload: web::Json<HashMap<NodeId, NodeInfo>>,
) -> impl Responder {
    let updates = payload.into_inner();
    for (node, info) in &updates {
        if let Some(baud) = info.uart_baud {
            serial.set_baud_rate(*node, baud).await?;
        }
    }
    bmc.set_node_info(updates).await?;
    Ok::<Null, LegacyResponse>(Null)
}

async fn get_node_aux_info(bmc: &BmcApplication) -> LegacyResult<serde_json::Value> {
    let infos = bmc.get_node_infos().await?;
    Ok(serde_json::to_value(infos)?)
}

async fn set_node_to_msd(bmc: &BmcApplication, query: Query) -> LegacyResult<()> {
    let node = get_node_param(&query)?;
    bmc.node_in_msd(node).await?;
    Ok(())
}

async fn read_os_release() -> std::io::Result<HashMap<String, String>> {
    let buffer = tokio::fs::read("/etc/os-release").await?;
    let mut lines = buffer.lines();
    let mut results = HashMap::new();

    while let Some(line) = lines.next_line().await? {
        if let Some((key, value)) = line.split_once('=') {
            results.insert(key.to_string(), value.to_string());
        }
    }
    Ok(results)
}

async fn read_hostname() -> io::Result<String> {
    let hostname = tokio::fs::read_to_string("/proc/sys/kernel/hostname")
        .await?
        .trim_end_matches(['\0', '\n'])
        .to_string();

    Ok(hostname)
}

async fn read_board_model() -> io::Result<(String, String)> {
    let info = ::board_info::BoardInfo::load()?;
    let board_model = info.value_of(&BoardInfoAttribute::ProductName);
    let board_revision = info.value_of(&BoardInfoAttribute::HwVersion);
    Ok((board_model, board_revision))
}

/// function is here for backwards compliance. Data is mostly a duplication of [`get_about`]
async fn get_system_information() -> impl Into<LegacyResponse> {
    let build_time = build_time::build_time_utc!("%Y-%m-%d %H:%M:%S-00:00");
    let ipv4 = get_ipv4_address().unwrap_or("Unknown".to_owned());
    let mac = get_mac_address("br0").await;

    let mut info = json!(
        {
            "api": API_VERSION,
            "buildtime": build_time,
            "ip": ipv4,
            "mac": mac,
        }
    );

    if let Ok(os_release) = read_os_release().await {
        let obj = info.as_object_mut().unwrap();
        if let Some(buildroot_edition) = os_release.get("PRETTY_NAME") {
            obj.insert(
                "buildroot".to_string(),
                serde_json::value::to_value(buildroot_edition).unwrap(),
            );
        }
        if let Some(build_version) = os_release.get("VERSION") {
            obj.insert(
                "version".to_string(),
                serde_json::value::to_value(build_version).unwrap(),
            );
        }
    }

    json!([info])
}

async fn set_node_power(bmc: &BmcApplication, query: Query) -> LegacyResponse {
    let parsed: PowerSetQuery = match parse_query_map(&query) {
        Ok(q) => q,
        Err(e) => return e,
    };

    let nodes = [
        ("node1", parsed.node1),
        ("node2", parsed.node2),
        ("node3", parsed.node3),
        ("node4", parsed.node4),
    ];

    let mut mask = 0;
    let mut states = 0;

    for (idx, (param, value)) in nodes.into_iter().enumerate() {
        let req_status = match value.as_deref() {
            Some("0") => false,
            Some("1") => true,
            Some(x) => {
                let msg = format!("Invalid value `{}` for parameter `{}`", x, param);
                return (StatusCode::BAD_REQUEST, msg).into();
            }
            None => continue,
        };
        let bit = 1 << idx;

        mask |= bit;

        if req_status {
            states |= bit;
        }
    }

    bmc.activate_slot(states, mask)
        .await
        .context("set power state")
        .into()
}

fn node_power_body(node1: &str, node2: &str, node3: &str, node4: &str) -> serde_json::Value {
    json!(
     [{
        "node1": node1,
        "node2": node2,
        "node3": node3,
        "node4": node4,
    }]
    )
}

async fn get_node_power(bmc: &BmcApplication) -> impl Into<LegacyResponse> {
    node_power_body(
        &get_node_power_status(bmc, NodeId::Node1).await,
        &get_node_power_status(bmc, NodeId::Node2).await,
        &get_node_power_status(bmc, NodeId::Node3).await,
        &get_node_power_status(bmc, NodeId::Node4).await,
    )
}

async fn get_node_power_status(bmc: &BmcApplication, node: NodeId) -> String {
    let Ok(status) = bmc.get_node_power(node).await else {
        return "Unknown".to_owned();
    };

    u8::from(status).to_string()
}

fn format_sdcard() -> impl Into<LegacyResponse> {
    LegacyResponse::not_implemented("microSD card formatting is not implemented")
}

/// function is here for backwards compliance. Data is mostly a duplication of [`get_info`]
fn get_sdcard_info() -> LegacyResponse {
    match get_fs_stat("/mnt/sdcard") {
        Ok((total, free)) => {
            let used = total - free;
            json!(
                 [{
                    "total": total,
                    "use": used,
                    "free": free,
                }]
            )
            .into()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            format!("Failed to get microSD card info: {e}"),
        )
            .into(),
    }
}

async fn set_node1_usb_mode(bmc: &BmcApplication, query: Query) -> LegacyResult<()> {
    bmc.set_node1_usb_route(query.contains_key("alternative_port"))
        .await
        .map_err(Into::into)
}

async fn get_node1_usb_mode(bmc: &BmcApplication) -> LegacyResult<serde_json::Value> {
    Ok(bmc.get_node1_usb_route().await?.into())
}

/// switches the USB configuration.
/// API values are mapped to the `UsbConfig` as followed:
///
/// | i32 | Mode         | Route |
/// |-----|--------------|-------|
/// | 0   | Host         | USB-A |
/// | 1   | Device       | USB-A |
/// | 2   | Flash host   | USB-A |
/// | 3   | Flash device | USB-A |
/// | 4   | Host         | BMC   |
/// | 5   | Device       | BMC   |
/// | 6   | Flash host   | BMC   |
/// | 7   | Flash device | BMC   |
///
async fn set_usb_mode(bmc: &BmcApplication, query: Query) -> LegacyResult<()> {
    let parsed: UsbSetQuery = parse_query_map(&query)?;
    let node_num = i32::from_str(&parsed.node)
        .map_err(|_| LegacyResponse::bad_request("Parameter `node` is not a number"))?;
    let node = NodeId::try_from(node_num).map_err(LegacyResponse::bad_request)?;
    let mode_num = i32::from_str(&parsed.mode)
        .map_err(|_| LegacyResponse::bad_request("Parameter `mode` is not a number"))?;

    let mode = UsbMode::from_api_mode(mode_num);

    let route = if (mode_num >> 2) & 0x1 == 1 {
        UsbRoute::Bmc
    } else {
        UsbRoute::AlternativePort
    };

    let cfg = match (mode, route) {
        (UsbMode::Device, UsbRoute::AlternativePort) => UsbConfig::UsbA(node),
        (UsbMode::Device, UsbRoute::Bmc) => UsbConfig::Bmc(node),
        (UsbMode::Host, route) => UsbConfig::Node(node, route),
        (UsbMode::Flash, route) => UsbConfig::Flashing(node, route),
    };

    bmc.configure_usb(cfg)
        .await
        .context("set USB mode")
        .map_err(Into::into)
}

/// USB mode as the legacy API sends it: one object in an array.
/// `node` is `NodeId::api_name` (`node1` … `node4`), not the Display form `Node N`.
fn usb_mode_body(config: UsbConfig, bus_type: &str) -> serde_json::Value {
    let (node, mode, route) = match config {
        UsbConfig::UsbA(node) => (node, UsbMode::Device, UsbRoute::AlternativePort),
        UsbConfig::Bmc(node) => (node, UsbMode::Device, UsbRoute::Bmc),
        UsbConfig::Node(node, route) => (node, UsbMode::Host, route),
        UsbConfig::Flashing(node, route) => (node, UsbMode::Flash, route),
    };

    json!(
        [{
            "mode": mode,
            "node": node.api_name(),
            "route": route,
            "bus_type": bus_type,
        }]
    )
}

/// gets the USB configuration from the POV of the configured node.
async fn get_usb_mode(bmc: &BmcApplication) -> LegacyResult<serde_json::Value> {
    let (config, bus_type) = bmc.get_usb_mode().await?;
    Ok(usb_mode_body(config, &bus_type))
}

async fn set_cooling_info(bmc: &BmcApplication, query: Query) -> LegacyResult<()> {
    let device = query
        .get("device")
        .ok_or(LegacyResponse::bad_request("Missing `device` parameter"))?;
    let speed_str = query
        .get("speed")
        .ok_or(LegacyResponse::bad_request("Missing `speed` parameter"))?;

    // check if the speed is a valid number within the range of the device
    let speed = c_ulong::from_str(speed_str)
        .map_err(|_| LegacyResponse::bad_request("`speed` parameter is not a number"))?;

    // set the speed
    bmc.set_cooling_speed(device, speed)
        .await
        .context("set Cooling state")
        .map_err(Into::into)
}

async fn get_cooling_info() -> LegacyResult<serde_json::Value> {
    let info = BmcApplication::get_cooling_devices().await?;
    Ok(json!(info))
}

async fn handle_flash_status(flash: web::Data<StreamingDataService>) -> HttpResponse {
    let status = flash.status().await;
    let code = StreamingDataService::flash_status_code(&status);
    match serde_json::to_string(status.deref()) {
        Ok(body) => HttpResponse::build(code)
            .content_type("application/json")
            .body(body),
        Err(e) => HttpResponse::InternalServerError().body(e.to_string()),
    }
}

async fn handle_transfer_request(
    ss: web::Data<StreamingDataService>,
    bmc: web::Data<BmcApplication>,
    query: web::Query<TransferQuery>,
) -> LegacyResult<String> {
    let (process_name, upgrade_command) = match query.ty.as_deref() {
        Some("firmware") => (
            "firmware upgrade service".to_string(),
            UpgradeCommand::OsUpgrade,
        ),
        Some("flash") => {
            let node = parse_node_id(query.node.as_deref())?;
            (
                format!("{node} os install service"),
                UpgradeCommand::Module {
                    node,
                    bmc: bmc.clone().into_inner(),
                },
            )
        }
        _ => {
            return Err(LegacyResponse::bad_request(
                "`type` should equal 'firmware' or 'flash'",
            ))
        }
    };

    let data_transfer = create_data_transfer(&query).await?;
    let do_crc = query.skip_crc.is_none();
    let transfer_request =
        InitializeTransfer::new(process_name, upgrade_command, data_transfer, do_crc);

    let handle = ss.request_transfer(transfer_request.try_into()?).await?;
    let json = json!({"handle": handle});
    Ok(json.to_string())
}

fn parse_node_id(node: Option<&str>) -> LegacyResult<NodeId> {
    let node_str = node.ok_or(LegacyResponse::bad_request("Missing `node` parameter"))?;
    let node_num = i32::from_str(node_str)
        .map_err(|_| LegacyResponse::bad_request("Parameter `node` is not a number"))?;
    NodeId::try_from(node_num).map_err(LegacyResponse::bad_request)
}

async fn create_data_transfer(query: &TransferQuery) -> LegacyResult<DataTransfer> {
    let file = query.file.as_deref().ok_or(LegacyResponse::bad_request(
        "Invalid `file` query parameter",
    ))?;

    if query.local.is_some() {
        let path = validate_local_transfer_path(Path::new(file))
            .map_err(|e| LegacyResponse::bad_request(e.to_string()))?;
        return Ok(DataTransfer::local(path));
    }

    let sha256 = try_map_sha256(query.sha256.as_ref())?;

    if file.starts_with("http://") || file.starts_with("https://") {
        let url = reqwest::Url::parse(file).map_err(|e| {
            LegacyResponse::bad_request(format!(
                "{file} could not be parsed to a url object: {:#}",
                e
            ))
        })?;
        validate_download_url(&url).map_err(|e| LegacyResponse::bad_request(e.to_string()))?;
        return Ok(DataTransfer::url(url, sha256).await?);
    }

    let size = query.length.as_ref().ok_or((
        StatusCode::LENGTH_REQUIRED,
        "Invalid `length` query parameter",
    ))?;

    let size = u64::from_str(size)
        .map_err(|_| LegacyResponse::bad_request("`length` parameter is not a number"))?;

    Ok(DataTransfer::remote(PathBuf::from(file), size, 16, sha256))
}

pub fn try_map_sha256(value: Option<&String>) -> LegacyResult<Option<bytes::Bytes>> {
    let sha = if let Some(sha256) = value {
        let bytes = hex::decode(sha256)
            .map_err(|e| {
                LegacyResponse::bad_request(format!(
                    "`sha256` parameter contains invalid hex values: {}",
                    e
                ))
            })?
            .into();
        Some(bytes)
    } else {
        None
    };

    Ok(sha)
}

#[get("/upload/{handle}/cancel")]
async fn cancel_file_upload(ss: web::Data<StreamingDataService>) -> impl Responder {
    ss.cancel_all().await;
    HttpResponse::Ok().finish()
}

#[post("/upload/{handle}")]
async fn handle_file_upload(
    handle: web::Path<u32>,
    ss: web::Data<StreamingDataService>,
    mut payload: Multipart,
) -> impl Responder {
    let (sender, size) = ss.take_sender(*handle).await?;
    let Some(Ok(mut field)) = payload.next().await else {
        return Err(LegacyResponse::bad_request("Multipart form invalid"));
    };

    let mut bytes_send: u64 = 0;
    while let Some(Ok(chunk)) = field.next().await {
        let length = chunk.len();
        if length > MAX_UPLOAD_CHUNK {
            ss.cancel_all().await;
            return Err(LegacyResponse::bad_request(format!(
                "upload chunk exceeds {}",
                format_size(MAX_UPLOAD_CHUNK as u64, DECIMAL)
            )));
        }
        if sender.send(chunk).await.is_err() {
            return Err(return_transfer_error(ss).await.into());
        }

        bytes_send += length as u64;
    }

    if bytes_send != size {
        ss.cancel_all().await;
        return Err(LegacyResponse::bad_request(format!(
            "missing {} bytes",
            format_size(size - bytes_send, DECIMAL)
        )));
    }

    Ok(Null)
}

/// When the channel gets dropped, give the worker some time to shutdown so that the
/// actual error message can be bubbled up.
async fn return_transfer_error(ss: web::Data<StreamingDataService>) -> impl Into<LegacyResponse> {
    let msg = ss.try_get_error(Duration::from_secs(5)).await;
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        msg.unwrap_or("transfer canceled".to_string()),
    )
}

#[cfg(test)]
mod test {

    use super::*;

    #[test]
    fn about_keeps_firmware_version_apart_from_daemon_version() {
        let os_release = HashMap::from([
            ("VERSION".to_string(), "2.3.4".to_string()),
            (
                "PRETTY_NAME".to_string(),
                "\"Buildroot 2026.08\"".to_string(),
            ),
        ]);
        let body = about_body(
            &os_release_fields(&os_release),
            "turingpi",
            "Turing Pi 2.5",
            "1.0",
            "2.4.0",
            "2026-11-06 10:00:00-00:00",
        );

        assert_eq!(body["version"], "2.3.4");
        assert_eq!(body["bmcd_version"], "2.4.0");
        assert_eq!(body["buildroot"], "Buildroot 2026.08");
        assert_eq!(body["api"], API_VERSION);
        assert!(body.get("build_version").is_none());
    }

    #[test]
    fn about_uses_unknown_when_os_release_has_no_version() {
        let body = about_body(
            &OsReleaseFields {
                version: None,
                buildroot: None,
            },
            "",
            "",
            "",
            env!("CARGO_PKG_VERSION"),
            "2026-11-06 10:00:00-00:00",
        );

        assert_eq!(body["version"], "unknown");
        assert_eq!(body["buildroot"], "unknown");
        assert_eq!(body["bmcd_version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn usb_mode_uses_api_node_name() {
        let body = usb_mode_body(UsbConfig::Node(NodeId::Node3, UsbRoute::Bmc), "Single bus");

        assert_eq!(body[0]["node"], "node3");
        assert_eq!(body[0]["mode"], "Host");
        assert_eq!(body[0]["route"], "Bmc");
        assert_eq!(body[0]["bus_type"], "Single bus");
    }

    #[test]
    fn node_power_uses_node1_to_node4_keys() {
        let body = node_power_body("1", "0", "1", "0");

        assert_eq!(
            body,
            json!([{
                "node1": "1",
                "node2": "0",
                "node3": "1",
                "node4": "0",
            }])
        );
    }

    #[actix_web::test]
    async fn test_node_info() {
        let json = serde_json::json! {
            {
                "Node1": {
                    "module_name": "Raspberry Pi CM4"
                },
                "Node3": {
                    "name": "New jeston"
                }
            }
        };
        let _: HashMap<NodeId, NodeInfo> = serde_json::from_value(json).unwrap();
    }
}
