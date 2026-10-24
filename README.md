# Live Keys

Plugin host (LV2 and VST3) focussed on playing keyboard live.

## Dev dependencies

```shell
sudo apt install liblilv-dev lv2-dev libsuil-dev
```

- `liblilv-dev`/`lv2-dev`: LV2 plugin hosting (`livi`/`lilv`).
- `libsuil-dev`: hosting LV2 plugins' own native GUIs (X11UI embedding via `suil`).

VST3 hosting needs no extra system packages.

## Previewing the UI

The `.slint` files under `crates/gui/ui/` can be previewed in the editor (e.g. the Slint
plugin's "Show Preview" in RustRover), filled with data from a real session in
`crates/gui/ui/mock_data.slint`. To regenerate that file from your own setup, run the app with
`LIVE_KEYS_MOCK_DATA=1 cargo run -p live-keys-gui --release`, set it up the way the preview
should show it, and quit.

The preview needs a language server at least as new as the project's Slint version (see
`Cargo.lock`). The one bundled with an editor plugin can be older, which shows up as errors
like "Element 'Platform' does not have a property 'uses-mock-data'". Install a matching one:

```shell
cargo install slint-lsp --version <Slint version> --locked
```

In RustRover, tick **Use external LSP server** in the Slint settings; the default command
finds `slint-lsp` on the `PATH` (`~/.cargo/bin`). Reinstall it whenever Slint is upgraded.
