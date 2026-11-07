# Keeping upstream files upstream

gpui-fast is a fork of GPUI, the UI framework in Zed's `crates/gpui`, together
with the Zed crates it depends on. Zed keeps changing those crates, and we want
to take their changes with a merge that mostly resolves itself. That only works
if our changes to upstream's files stay small: a file we rewrote conflicts on
every sync, a file with a one-line hook almost never does.

So gpui-fast's code lives in files upstream doesn't have, and upstream's files
only hold the hooks that call into it.

## The rules

1. **gpui-fast's logic lives in `crates/<crate>/src/fast/`.** One file per
   topic: `crates/gpui/src/fast/retained.rs`, `fast/dependencies.rs`,
   `fast/layout.rs`, and so on. A new topic gets a new `fast/<topic>.rs`, or
   `fast/<topic>/` when it needs more than one file. Other upstream crates get
   their own `src/fast/` when they need one. Tests of what we add go in
   `crates/gpui/src/fast/tests/<topic>.rs` or at the bottom of the topic's file.
2. **Upstream files only hold small hooks, and every hook names `fast`:**
   - one field holding the topic's state, typed as a struct defined in `fast/`
     (`pub(crate) fast_layout: crate::fast::layout_key::WindowLayout`) and
     initialized by its path (`crate::fast::layout_key::WindowLayout::default()`);
   - a one-line call into `crate::fast::...`, or a method whose body only
     forwards to one. The call names the path even where a method would read
     shorter: `crate::fast::dependencies::note_notify(&mut self.entities, id)`,
     not `self.entities.note_notify(id)`, and
     `crate::fast::dependencies::StateVersion::bump(&state.version)`, not
     `state.version.bump()`. Whoever merges upstream can then tell every line
     of ours from upstream's at a glance;
   - a visibility bump (`fn` to `pub(crate) fn`) so a `fast/` module can reach
     upstream's items. Methods of upstream types can be defined in an
     `impl Window { ... }` block inside a `fast/` file for `fast/` code to
     call, but an upstream file calls a free function in `fast/` instead, so
     the call site shows where the code lives;
   - where no path fits — a field, parameter or local of a plain type that a
     hook threads through upstream code — an identifier named `fast_...`
     (`fast_layout_key: u64`);
   - `mod` lines;
   - a `#[path = "fast/<file>.rs"] mod <name>;` redirect when we replaced a
     whole upstream file with our own rewrite. The upstream file then stays
     exactly as upstream has it, unused.
3. **No new types, algorithms, bookkeeping or tests in upstream files.** No
   reformatting, reordering or renaming of upstream code either: code we don't
   need to change stays byte-for-byte upstream's.
4. **Name fast code by its full path.** Outside `fast/`, write
   `crate::fast::<topic>::Name` where it is used, never a
   `use crate::fast::…` line, so every hook shows where its code lives. The
   one exception is `gpui.rs` exporting a `test-support` item one at a time
   (rule 5). Inside `fast/`, listing names in a `use` line is fine. Glob
   imports or re-exports of `fast` (`pub use fast::*`,
   `use crate::fast::layout::*`) are never allowed.
5. **No new public API.** gpui-fast changes how GPUI draws, not what it offers:
   its public API is upstream's. What tests and `gpui_perf` need to measure or
   switch retained mode is compiled only under `test-support`, and exported
   from `gpui.rs` one item at a time:
   `#[cfg(any(test, feature = "test-support"))] pub use fast::stats::LayoutStats;`.

   The one deliberate exception is `fast::dynamic_textures` (longbridge/gpui-fast#86):
   content that changes every frame — video, remote desktop — needs an app-visible
   mutable texture, and upstream offers nothing of the kind; without one, each
   frame reallocates atlas tiles and invalidates the glyphs sharing them. The
   surface stays at the minimum that makes the feature usable: the opaque
   `DynamicTexture` handle and `DynamicTextureId`, the `DynamicTexturePlatform`
   trait and its installer (the seam the platform crates implement), five
   `Window` methods, and `DynamicTexture::new_gpu`, `id` and `remove` for
   renderer crates. The wgpu crate adds an
   owning GPU texture type and its context constructor under the same
   `fast::dynamic_textures` exception. Everything else is `pub(crate)`, and the
   module adds no rendering behavior an app could observe without calling the
   new methods.
   A second exception is `WebPlatform::wgpu_context()` (in
   `gpui_web::fast::wgpu_context`, longbridge/gpui-fast#102): a browser page has
   no window composition, so a wgpu-rendered element there can only reach the
   screen by drawing during `paint` into a `WgpuDynamicTexture` allocated from
   the context that draws the window. `WebPlatform` kept that context private,
   which left the feature unreachable to an application that does not replace
   the platform.
   The addition is one accessor returning a clone of the `Rc` it already
   holds, and nothing else about the web platform becomes public.
6. **New files only inside `fast/`, or in our own crates** such as
   `crates/gpui_perf` (benchmarks, examples and the frame-measuring app) and
   `compat/` (the `gpui-pre-*` stand-ins GPUI Kit applications patch in).
   Documentation goes in `docs/`.

Which directories are upstream's is recorded in [`UPSTREAM`](../UPSTREAM) at
the repository root: every directory in `crates/` except `gpui_perf`, and
`tooling/perf`.

## Checking

```sh
script/check-upstream                   # compare with upstream as imported into our history
script/check-upstream --zed ~/github/zed  # compare with a zed checkout at the recorded commit
script/check-upstream --report          # print the table without failing
script/check-upstream --all             # also list the changed files that pass
```

The script compares every file in the upstream directories with upstream's
version of it: by default `git show <import_commit>:<path>` from our own history,
or with `--zed` the same path in a zed checkout at `zed_commit`. It checks the
working tree, so run it before committing. It fails when:

- a file outside `src/fast/` is added to, or removed from, an upstream
  directory (`LICENSE*` files excepted);
- a hunk of a changed upstream file adds more than 8 lines (`--max-hunk`);
- a changed upstream file adds more than 40 lines (`--max-added`) or removes
  more than 20 (`--max-removed`);
- a hunk of a changed upstream `.rs` file adds lines none of which names
  `fast`: a path such as `crate::fast::...`, `mod fast;` or
  `#[path = "fast/..."]`, or an identifier starting with `fast_`. One mention
  covers the hunk, since rustfmt may spread one call over several lines. Hunks
  that only remove lines, or only change `use` declarations or blank lines,
  are exempt;
- a line added to an upstream `.rs` file calls a method on a value named
  `fast_...` (`self.fast_layout.end_frame()`): the name marks the hunk but
  hides which module the method is in, so the hook calls it by its path,
  `crate::fast::layout_key::WindowLayout::end_frame(&mut self.fast_layout)`;
- a binary file differs;
- any file glob-imports from `fast` (`use ...fast::*`,
  `use ...fast::<topic>::*`), or a file inside `fast/` has a glob import of
  any kind, `use super::*` in a test module included;
- a file outside `fast/` has a `use` of `fast` at all (`use crate::fast::…`,
  `pub(crate) use …`): only a public `pub use fast::…` export passes.

Apart from the glob rules, files under any `src/fast/` directory are never
checked. A line that differs from upstream's only by a visibility bump
(`pub(crate)`, `pub(super)`) is a hook by definition: it is counted in the
table's `pub(crate)` column and not against any budget.

A justified exception goes in `script/upstream-allowlist`: one line per path or
glob, optionally raising the budgets (`hunk=N`, `added=N`, `removed=N`) or
allowing any change (`any`), always with a reason. The usual reasons are a hub
file with many one-line hooks, and an upstream body replaced by a `fast/`
implementation it now forwards to:

```text
crates/gpui/src/view.rs removed=210 # ViewElement's cache-by-bounds is replaced by fast::retained's retained views
```

Removed lines deserve the most care: upstream's changes to code we deleted
conflict on every sync, and have to be ported into `fast/` by hand.

### Where our API differs from upstream's

The check cannot see a change to what upstream's public types offer, so the
few places where gpui-fast's differs from upstream's are listed here. Each is
forced by what `fast/` keeps; anything not listed here is upstream's API
unchanged, and a new entry needs as good a reason.

- `ViewElement`'s `Element::RequestLayoutState` and `PrepaintState` are
  `fast::retained::ViewLayoutState` and `ViewPrepaintState`, opaque types, in
  place of `Option<AnyElement>`. `ViewElement` is `#[doc(hidden)]`, and the
  states are only ever handed back to it by GPUI.
- `Scene` has a public `layers` field (`fast::layers::scene::SceneLayers`),
  and `gpui` exports `LayerKey`, `TileCoord`, `SceneLayers`, `LayerFrame`,
  `LayerContent` (a layer's content, in parts: one per row of a virtual
  list), `LAYER_TILE_TEXTURE_BASE`, `layer_tile_texture_id`, `layer_tile_id` and
  `decode_layer_tile`, because renderers live in other crates and read scroll
  layers from the scene. Polychrome sprites whose texture index is at or
  above `LAYER_TILE_TEXTURE_BASE` are renderer layer references, not atlas
  textures. Reserved ids from `0xF100_0000` identify rounded overflow groups
  with `tile_size = 0`; their drawable scene carries padding-edge geometry
  in a non-composited frame keyed by `u32::MAX`. The private wire format lives
  in `fast::rounded_clip_scene`; Metal and WGPU evaluate the clip
  stack directly without intermediate render targets.
- `crates/gpui/Cargo.toml` names this repository and sets `publish = false`.
- Window composition, the API of
  [zed#62379](https://github.com/zed-industries/zed/pull/62379), which
  upstream has not merged yet: native views drawn between a window's GPUI
  content and its overlays. `gpui` exports `WindowComposition`,
  `WindowCompositionSurface`, `CompositionSurfaceId`,
  `CompositionSurfaceKind`, `PlatformSurfaceAttachment`,
  `PlatformCompositionSurface`, `PlatformCompositionSurfaceContent`,
  `ComposedScene` and `ComposedSceneLayer`; `PlatformWindow` gains
  `draw_composed`, `enable_window_composition`, `create_native_surface` and
  `set_composition_order`, with defaults that keep a backend without
  composition working; `Scene` gains `is_empty` and `replay_balanced`, and
  `ComposedScene::layer_scene`, ours, cuts one layer's scene out for a
  renderer; `Window` gains `enable_window_composition` and
  `with_composition_surface`; `WgpuRenderer` gains `new_sharing_atlas` and
`replace_surface_sharing_context` for `gpui_linux`, and `Background` gains
`is_opaque` so `gpui_linux` cuts only opaque content out of native surfaces. It all lives in `fast::composition`, so when
  upstream merges its version, the sync replaces ours with it. Two things
  behave differently from the pull request: a retained view drawn again from
  last frame keeps the surface switches it made, and `with_composition_surface`
  inside a scroll layer paints where the layer does. A window that composes
  but holds no surface beyond the base and overlay surfaces presents with
  `PlatformWindow::draw` and gives the platform only its base surface, so the
  overlay's view, layer and textures are released until a surface is added.
  On X11, when a
  compositing manager blends ARGB windows, the GPUI content above native
  surfaces is drawn into a transparent override-redirect window over the
  window instead of being cut out of them, so shadows and translucent
  backdrops blend over native content; `X11Client::get_window` falls back to
  the window such an overlay window covers. The overlay content is drawn into
  the window too, with its opaque parts cut out, so it still shows when the
  compositor stacks the overlay window below the window (Hyprland renders
  pinned windows above all others). The `native_webview` example in
  `gpui_perf` shows composition on macOS, Windows and Linux, and
  `linux_webview` shows a WebKitGTK page under overlays on X11.

Composition input extends that existing exception: `ComposedScene::layer_hitboxes`
lets the separate platform crates use prepared GPUI hitboxes instead of inferring
input from graphics. `WindowComposition::on_input` registers a cooperating
windowless producer; the defaulted `PlatformSurfaceAttachment::input_forwarding`
reader carries that choice through the existing platform-order update. Existing
attachment implementations and platform surface struct initializers stay valid. OS-managed child views keep
their platform event loop. Capture runs first, press ownership survives through
release, and leave is sent when the pointer changes recipients. Prepaint scopes
and retained context keep input ownership with the same surface as its paint.
No application-specific timeline policy belongs here.

- Dynamic textures (longbridge/gpui-fast#86), ours to change until upstream
  takes them:
  `gpui` exports `DynamicTexture`, `DynamicTextureId`,
  `DynamicTexturePlatform`, `set_dynamic_texture_platform` and `Window`
  gains `create_dynamic_texture`, `update_dynamic_texture`,
  `paint_dynamic_texture` and `remove_dynamic_texture`. Streaming content —
  video, remote desktop — changes every frame, and upstream's only path for
  pixels is the atlas, where each frame's upload reallocates tiles and
  invalidates the glyphs sharing them. The handle keeps its fields private and
  the id only exposes the atlas-index mapping the renderer crates need to
  resolve a sprite; the trait and its installer exist so the platform crates
  can register their backends, and everything else in
  `fast::dynamic_textures` is `pub(crate)`; without calling the new methods,
  an app observes no difference in what GPUI draws.

  Content rendered on the window renderer's device can use
  `WgpuContext::create_dynamic_texture` instead of an intermediate image.
  Its `WgpuDynamicTexture` owner provides `render` and `paint`: the producer
  submits on the shared queue, then the ordinary sprite pipeline samples
  that same texture, with no upload or transfer copy. A successful write
  advances the content version and, outside a frame, schedules a frame
  that draws again only the retained views and scroll-layer tiles that
  painted the texture (they read a per-texture `StateVersion` when they
  paint it); a texture nothing painted yet still refreshes the window.
  CPU updates through `Window::update_dynamic_texture` behave the same.
  `render_srgb`, `format` and `srgb_view_format` let a producer that
  computes linear color, such as a lit 3D scene, write through an sRGB view
  of the same texture where the device can reinterpret formats; GPUI keeps
  sampling the plain view, as it samples image bytes.

  Each context owns its texture lookup table; clones share it, while a new
  device starts with an empty table. The owning handle removes its entry
  and retires its core metadata on drop. GPU objects never enter the global
  upload platform, so this ownership works on browser WebGPU and WebGL as
  well as native wgpu. The Metal upload backend and its installer build only
  for iOS, with the Metal renderer; CPU updates refuse GPU-written metadata.
  A native Metal producer interface is outside this extension.
  On native wgpu, publishing a window context (see below) installs a CPU
  upload backend: `Window::create_dynamic_texture` textures are wgpu
  textures on that context's device, registered in its lookup table. A
  write goes into a CPU copy of the texture and widens its dirty region,
  and the next frame on the device uploads that region with
  `Queue::write_texture` before it submits, so a window that does not draw
  stages nothing however often the app writes. After device loss the
  textures are recreated on the new device with the same ids, sizes and
  contents.

- The window renderer's wgpu context, for applications (`gpui_wgpu::fast::app_context`):
  `gpui_wgpu` exports `app_context()`, `WgpuAppContext` and
  `WgpuContextGeneration` on native targets. A `WgpuDynamicTexture` must come
  from the context drawing the window, which the platform keeps private, so
  without this an application cannot render into a texture GPUI samples. The
  renderer publishes the context it creates for the first window and the one
  it creates after device loss; the generation tells the application to
  recreate its GPU objects.
  `gpui_wgpu` also exports `set_adapter_preference`, `WgpuAdapterPreference`
  and `describe_adapters`: an application that used to choose the adapter
  of its own wgpu device (by name, or a software adapter) now renders on
  GPUI's, so it steers the adapter GPUI picks for windows and recovery.

- Native wgpu windows on Windows and macOS (`gpui_wgpu::fast::native_surface`):
  `WgpuContext::instance` selects DX12 on Windows and Metal on Apple targets
  (Vulkan and GL elsewhere, as upstream). `gpui_wgpu` exports the
  `WgpuSurfaceSource` trait, and `WgpuRenderer` gains `new_from_source`,
  `recover_from_source`, `new_sharing_atlas_from_source`,
  `replace_surface_from_source` and `reconfigure_surface`, which reports a
  failed swap chain configure so the platform can retry it. Upstream only
  builds surfaces from raw window handles, while DirectComposition visuals
  and `CAMetalLayer`s need surfaces the platform creates, on whichever
  instance owns the device.
  `WgpuRenderer` also gains `set_clear_color`, `set_text_rendering_params`
  (with the exported `WgpuTextRenderingParams`) and `set_color_space`, which
  those platforms' renderers set from the OS (an opaque clear color under a
  composition swap chain's fixed alpha mode, DirectWrite's text parameters,
  Display P3), and a `test-support` `render_to_image` for their visual tests.
  On Apple targets the default font gamma is 1.0 and the grayscale contrast
  0.0, because Core Text coverage is already gamma-adjusted.

- macOS windows draw through `gpui_wgpu` (`gpui_macos/src/fast/renderer.rs`,
  a `CAMetalLayer` per window and per composition overlay). `gpui_macos` no
  longer exports `metal_renderer` (`MetalHeadlessRenderer`,
  `PathRasterizationVertex`, `PathSprite`, `SurfaceBounds`), and
  `gpui_platform::current_headless_renderer` returns `gpui_wgpu`'s
  `WgpuHeadlessRenderer` on macOS and Windows (DX12) as on Linux
  (`gpui_platform::fast::headless`). `gpui_apple` builds its Metal renderer,
  atlas, shaders and Metal dynamic texture backend only for iOS: its build
  script sets the `fast_metal_renderer` cfg there
  (`gpui_apple/src/fast/metal_renderer_cfg.rs`), so a macOS build runs no
  `xcrun metal` and can be checked from any host, and `gpui_macos`'s
  `runtime_shaders` feature has no effect. Windows pace frames with the
  `CVDisplayLink` registry on every macOS version, never a
  `CAMetalDisplayLink`, because wgpu acquires the layer's drawables itself
  (`gpui_macos::fast::metal_frames`). `gpui::surface` content
  (`PaintSurface`, a `CVPixelBuffer`, as macOS screen capture frames are
  delivered) is not drawn on macOS: `gpui_wgpu` has no surface pipeline, and
  the renderer logs a warning the first time a scene holds one. On a sync,
  `gpui_macos.rs` keeps redirecting its `renderer` module to
  `fast/renderer.rs` in place of `gpui_apple::metal_renderer`: when upstream's
  `gpui_macos` starts calling a `MetalRenderer` method, add it there.

When the check fails, move the change into a `fast/` module and leave a hook
behind that names it; use `git diff <import_commit> -- <file>` to see what
differs.

## Syncing with upstream

Upstream is [zed-industries/zed](https://github.com/zed-industries/zed). The
commit our copy was taken from is `zed_commit` in `UPSTREAM`, and
`import_commit` is our commit holding that copy unchanged. To take a newer zed:

1. In a zed checkout, pick the new commit and run
   `script/import-upstream --zed <checkout> <commit>`. It starts a vendor
   branch at `import_commit` and replays each zed commit since `zed_commit`
   that touches a directory listed in `UPSTREAM`, limited to those
   directories and keeping its author, dates and message, so zed's history
   of those crates comes along. The branch holds nothing but upstream's code;
   its last commit is the new vendor commit.
2. Merge that branch into ours. Conflicts should only touch hooks; resolve them
   by keeping upstream's code and putting our hook back. The pull request must
   be merged with a merge commit, not squashed: the next sync's merge needs
   the vendor commits in `main`'s history.
   Name our sync commit `chore: Sync GPUI upstream <zed-commit>`, using the
   short target Zed commit, for example `chore: Sync GPUI upstream a1b71072e5`.
   Do not start its title with `Merge`; replayed vendor commits keep their
   original upstream messages.
3. For every file we redirect with `#[path = "fast/..."]`, look at what
   upstream changed in the original (`git diff <old vendor commit> <new vendor
   commit> -- <file>`) and port it into our copy by hand. The merge won't
   conflict on those files, so this step is easy to forget.

   `gpui_windows`' `directx_renderer` module is one of them: windows draw
   through `gpui_wgpu` on Direct3D 12 (`gpui_windows/src/fast/renderer.rs`),
   so upstream's Direct3D 11 renderer (`directx_renderer.rs`) and its atlas
   (`directx_atlas.rs`, whose `mod` line is removed) are kept as upstream
   has them but not compiled. `build.rs` is upstream's too, and release
   builds still compile `shaders.hlsl` with `fxc` next to the color glyph
   shaders. Port only what our `fast/directx_renderer.rs` keeps of the
   original: DirectWrite's rendering parameters (`get_font_info`), the color
   glyph shaders, and the GPU description (`gpu_specs` with its NVIDIA, AMD
   and DXGI driver version queries). A rendering fix upstream makes to its
   Direct3D 11 renderer belongs in `gpui_wgpu`, if that renderer needs it
   too.
4. Update `zed_commit` and `import_commit` in `UPSTREAM` to the new zed commit
   and the new vendor commit, and add or remove entries under `tracked` if zed
   added or removed crates we use.
5. Look at whether upstream changed how an index under the pointer is found:
   `TextLayout::index_for_position`'s steps from line to line (with
   `WrappedLineLayout::size` and `WrappedLine::len`),
   `WrappedLineLayout::_index_for_position` and `LineLayout::index_for_x`
   (`git diff <old vendor commit> <new vendor commit> --
   crates/gpui/src/elements/text.rs crates/gpui/src/text_system/line.rs
   crates/gpui/src/text_system/line_layout.rs`). `fast/interactive_text.rs`
   follows those steps to lay hitboxes over where an `InteractiveText` shows
   the hand, while its clicks still go through upstream's code; port the
   change into it, or the hand and the clicks part. Nothing conflicts there
   either.
6. Run `script/check-upstream`, the tests and clippy.
7. Check that `window/a11y.rs` still keeps the tree it last sent in
   `A11yDebug::last_tree_update` in every build: `fast/a11y.rs` reads the
   nodes it draws again from there
   (`a11y_frame_keeps_the_tree_it_sent` fails if it stops).
