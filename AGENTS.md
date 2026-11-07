# bmcd

BMC daemon for a Turing Pi board. It serves the legacy HTTP API under `/api/bmc` and the static BMC-UI tree from its `www` directory. [BMC-UI](https://github.com/turing-machines/BMC-UI) is that UI. [BMC-Firmware](https://github.com/turing-machines/BMC-Firmware) builds this crate into the board image.

Human build and install steps are in [README.md](README.md).

## Sibling repos

| Repo | Owns | Local checkout |
| --- | --- | --- |
| bmcd (this repo) | The daemon and the HTTP API | this directory |
| [BMC-UI](https://github.com/turing-machines/BMC-UI) | The web UI | `../BMC-UI` when checked out beside this repo |
| [BMC-Firmware](https://github.com/turing-machines/BMC-Firmware) | The board image | `../BMC-Firmware` when checked out beside this repo |

A UI bug filed on BMC-Firmware is fixed in BMC-UI when the screen is wrong, and here when the JSON or the daemon behavior is wrong.

## API

Legacy handlers live in `bmcd/src/api/legacy.rs`. Responses that carry a body use `{ "response": [{ "result": ... }] }` from `LegacyResponse`.

`type=about`: `version` is os-release `VERSION` (firmware). `bmcd_version` is `CARGO_PKG_VERSION`. A missing os-release file yields `version` and `buildroot` of `"unknown"`. The builder is `about_body`.

`type=usb`: `usb_mode_body` returns a one-element array. `node` is `NodeId::api_name` (`node1` … `node4`). `Display` for the same id is `Node 1` … `Node 4`. Keep those two spellings apart.

`type=power`: `node_power_body` returns a one-element array. Keys are `node1` … `node4`. Values are `"0"`, `"1"`, or `"Unknown"`.

`type=firmware` and `type=flash`: `StreamingState` serializes as `{"Error":"…"}`, `{"Done":[{"secs":n,"nanos":n}, bytes]}`, or `{"Transferring":{…}}`. `flash_status_code` is HTTP 500 for `Error` and 200 otherwise. BMC-UI also accepts an `Error` body on HTTP 200.

## Tests without a board

`cargo test` does not open GPIO. The real `PowerController::new` opens `/dev/gpiochip1` or `/dev/gpiochip2`, so a test that constructs it belongs behind `--features stubbed`. That feature swaps in `bmcd/src/hal/stub/power_controller.rs` and `pin_controller.rs`, which log and return success.

`gpiod` is a Linux-only dependency. On macOS the default build fails while resolving that crate. Run the suite in a Linux container with `libusb-1.0-0-dev` and `libssl-dev`, and keep `CARGO_TARGET_DIR` in a Docker volume so the host tree stays free of Linux build artifacts:

```bash
docker run --rm -v "$PWD":/src -w /src -e CARGO_TARGET_DIR=/cargo-target -v bmcd-cargo-target:/cargo-target rust:1.88-bookworm \
  bash -c "apt-get update && apt-get install -y libusb-1.0-0-dev libssl-dev pkg-config && cargo test"
```

USB flashing, `osupdate`, and the GPIO chip still need a Turing Pi. The `stubbed` feature does not stand in for those.

Rust is `rust-version` in `bmcd/Cargo.toml`. The `vendored` feature statically links OpenSSL for a standalone debug binary. The firmware build leaves OpenSSL dynamic. The cross-compile command is in the README.

## CI

`.github/workflows/cargo_ci.yml` runs on every push:

- `cargo clippy -- -D warnings` after installing `libusb-1.0-0-dev`
- `cargo test`
- `cargo deny` for bans, licenses, and sources

## Board check

The container suite never opens GPIO, USB, or `osupdate`. Once on a Turing Pi, with BMC-UI pointed at the board:

- Fail a firmware upgrade. The status line and the toast show the daemon message, and the progress bar clears.
- About and the header show os-release `VERSION`. The daemon version is the separate `bmcd_version` field.
- The USB page opens on the node the daemon reports, on a module that is not node 1. Power keys stay `node1` … `node4`.
- At about 768–800 px wide, the six BMC-UI tabs stay on one line. That check has no daemon side.

## Done

A behavior change leaves `cargo test` green on Linux, including the about, USB node name, power keys, and flash-status JSON tests when those shapes move. `cargo clippy -- -D warnings` passes. An API field the UI reads is updated in BMC-UI `src/lib/api/contracts` in the same change.
