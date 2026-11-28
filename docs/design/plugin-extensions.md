# Plugin extensions for later

Host-side callbacks and extensions that installed plugins ask for and we don't
implement yet, judged not worth it for now (surveyed 2026-11-28: LV2 features
declared in every installed `.ttl`, CLAP extension ids in every `.clap`
binary). The ones that were worth it — the LV2 worker, tempo and transport,
VST3 `restartComponent(kParamValuesChanged)`, CLAP remote controls and
parameter indication — are built; see `CLAUDE.md`.

## CLAP

- **`clap.thread-pool`** (Surge XT, Vital): the plugin asks the host to run
  per-voice work on several threads. Only CPU use; needs a real-time-safe pool
  of our own.
- **`clap.track-info`** (Surge XT, Vital): the slot's name and colour, which
  the plugin may show. Cosmetic.
- **`clap.context-menu`** (Surge XT, Vital): host items in the plugin's own
  right-click menu, and the plugin's items in ours. A natural place for "MIDI
  learn this parameter" later.
- **`clap.state-context`**: tells a plugin whether state is saved for a preset
  or a project. Ours are always both.
- **`clap.voice-info`**, **`clap.note-name`**, **`clap.audio-ports-config`**,
  **`clap.audio-ports-activation`**, **`clap.resource-directory`**,
  **`clap.render`**: not needed for a live rack as it is.
- **`clap.latency`** and **`clap.tail`**: we have no latency compensation, and
  every plugin is processed every block, so neither changes anything yet.

## LV2

- **`log:log`** (11 plugins, optional): their messages would go to our log
  instead of stderr.
- **`ui:touch`** (194 GUIs, mostly LSP): a GUI says when the user grabs and
  lets go of a control. Could pause MIDI learn's takeover while the mouse holds
  a control.
- **`ui:portSubscribe`**, **`ui:peakProtocol`**: every output already goes to
  an open GUI; no installed plugin needs these.

## VST3

- **`IUnitInfo` program lists**: factory presets some plugins expose as
  programs aren't listed with the `.vstpreset` files.
- **`IComponentHandler2/3`** (`setDirty`, `requestOpenEditor`, group edits, the
  host's context menu): nothing uses them here yet.
- **`IPlugInterfaceSupport`**: lets a plugin ask which host interfaces exist.
- **Latency** (`restartComponent(kLatencyChanged)`): no compensation, as above.
