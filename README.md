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

## Run

```sh
cargo run
```

The app opens a simple GPUI window with a `Quit` button.
