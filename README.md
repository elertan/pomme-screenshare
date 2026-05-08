# Pomme Screenshare

Small Rust desktop app starter using [GPUI](https://www.gpui.rs/), the UI framework used by Zed.

## Requirements

- Latest stable Rust
- macOS or Linux
- On macOS: Xcode and Xcode command line tools for Metal support

If the first build fails with `missing Metal Toolchain`, install the Xcode component:

```sh
xcodebuild -downloadComponent MetalToolchain
```

## Run Client

```sh
cargo run -p pomme-screenshare-client
```

The app opens a GPUI window with a `Connect` button and a full-window video
canvas for decoded frames.

## Run Server

```sh
cargo run -p pomme-screenshare-server
```

The server listens on `0.0.0.0:1337` and relays length-prefixed video packets
to every connected client except the sender.
