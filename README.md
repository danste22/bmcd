# bmcd

`bmcd` or 'BMC Daemon' is part of the
[BMC-Firmware](https://www.github.com/turing-machines/BMC-Firmware) and is
responsible for hosting Restful APIs related to node management, and
configuration of a Turing-Pi 2 board.

The web UI it serves is [BMC-UI](https://github.com/turing-machines/BMC-UI). Agent instructions for building, testing, and changing this repo are in [AGENTS.md](AGENTS.md).

## Building

This package will be built as part of the buildroot firmware located
[here](https://www.github.com/turing-machines/BMC-Firmware). If you want to
build bmcd in isolation, we recommend to use `cargo cross`. Given you have a
Rust toolchain installed, execute the following commands:

```bash
# Install cross environment
cargo install cross --git https://github.com/cross-rs/cross

# Execute cross build command for the Turing-Pi target.
cross build --target armv7-unknown-linux-gnueabi --release --features vendored
# A self contained binary is build when the "vendored" feature flag is defined.
# i.e. Openssl will be statically linked into the binary. This is not desirable
# when building the actual BMC-Firmware, but works great for debugging scenario's.

# Copy to turing-pi.
scp target/armv7-unknown-linux-gnueabi/release/bmcd root@turingpi.local:/usr/bin/
```

## Testing

`cargo test` covers the daemon logic that does not touch the board: the about payload, USB node names, power keys, and firmware-status JSON. It needs Linux, because `gpiod` does not build on macOS. GitHub runs that suite from `.github/workflows/cargo_ci.yml` on every push.

A Linux container keeps the build off the host disk. Put the Cargo target directory in a volume (`CARGO_TARGET_DIR`) and install `libusb-1.0-0-dev`, `libssl-dev`, and `pkg-config` in the image. The exact command is in [AGENTS.md](AGENTS.md).

`--features stubbed` replaces the GPIO power and pin controllers with in-process stand-ins. USB flashing, `osupdate`, and the real GPIO chip still need a Turing Pi.

