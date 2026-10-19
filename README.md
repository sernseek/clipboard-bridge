# clipboard-bridge

A compatibility daemon for clipboard transfers between Wayland and X11/XWayland
on Linux. A single Rust process monitors both protocols and applies explicit
format-selection and file-list normalization policies.

## How this differs from clipboard tools

| Component | Role |
| --- | --- |
| `wl-copy` | Writes data to a Wayland clipboard. It supports images and arbitrary MIME content, and normally stays in the background to serve paste requests. |
| `wl-paste` | Reads a Wayland clipboard, or watches it with `--watch` for use by another command. |
| Compositor/XWayland integration | Provides normal cross-protocol selection forwarding. For example, Niri uses xwayland-satellite, which already synchronizes Wayland and X11 selections. |
| `clipboard-bridge` | Runs continuously as an additional compatibility layer: chooses supported content, supplies MIME aliases, and normalizes file copy/cut representations. |

`xclip` and `xsel` are analogous command-line tools for X11. This daemon is not a
replacement for these utilities, a compositor, or a clipboard-history manager.
It does not provide their general-purpose command-line interfaces.

**Native synchronization is the default, not a missing feature this program
adds to every desktop.** If the compositor's integration already handles your
applications correctly, prefer that integration and ordinary clipboard tools;
there is no reason to run another bridge just to copy an image.

Use this daemon only for a reproducible format/interoperability need. Its
policies deliberately support a subset of clipboard formats, rather than
transparently proxying every offer. Another selection owner can interact with
the native bridge, so verify the real application workflow before enabling it.
It does not guarantee that every application accepts every pasted format.

The upstream [wl-clipboard manual](https://github.com/bugaevc/wl-clipboard/blob/HEAD/data/wl-clipboard.1)
describes the utilities, including MIME selection and watch mode. The
[xwayland-satellite architecture](https://github.com/Supreeeme/xwayland-satellite/blob/HEAD/ARCHITECTURE.md)
describes its native Wayland/X11 selection synchronization.

## Behavior

- Synchronizes UTF-8 text, images, and file/folder lists in both directions.
- Prefers PNG, then another offered image type, over an image's HTML/text companions.
- Keeps an existing native Wayland HTML/rich-text offer when the peer already
  contains the same text, instead of replacing it with plain text.
- Preserves file copy/cut through `x-special/gnome-copied-files`, `text/uri-list`,
  and `application/x-kde-cutselection`. URI encoding and order are preserved.
- Identifies bridge-created offers with `application/x-nixos-clipboard-bridge`;
  echoes do not invalidate a newer genuine copy. Identical re-copies are allowed
  without a content-hash TTL.
- Rejects stale transfers using selection serials and a shared generation guard.
  Unsupported formats and clears invalidate pending work but are not replayed or
  automatically written over the other clipboard.
- Handles X11 INCR in both directions, concurrent consumers, early-closing
  consumers, and sources that stop responding. INCR size hints are advisory lower
  bounds; actual received bytes enforce the limit, even with an empty size hint.

File synchronization transfers **URI metadata**, not file contents. The receiving
application must support file paste and be able to access the original paths;
it copies or moves the files when the user pastes. This does not bypass sandbox
or filesystem permissions and does not bridge portal-specific transfer tokens.

If a file is pasted as literal `file://...` text, inspect the offered MIME types.
File-list types must survive throughout the clipboard chain; a text-only
forwarder can replace them with plain text. Stop superseded clipboard
synchronization services before enabling this daemon.

Reads and individual outgoing transfers have a 5-second deadline. Limits are
512 KiB for text, 64 MiB for encoded images, and 4 MiB for file-list metadata;
the latter is not a limit on the size of the referenced files. Transfer buffers
are shared between threads and concurrent readers, not copied once per consumer.

## Requirements

- A Wayland compositor exposing `zwlr_data_control_manager_v1` (version 1 or 2).
  Niri and Sway are supported. Ext-data-control-only compositors are not supported.
- An accessible X11/XWayland server with XFixes selection notifications.
- The graphical session's `DISPLAY`, `WAYLAND_DISPLAY`, and `XDG_RUNTIME_DIR`,
  plus `XAUTHORITY` when required by the X server.

The first advertised Wayland seat is used. Only CLIPBOARD is synchronized, not
PRIMARY. Run one bridge instance per graphical session. The daemon owns the
forwarded data while running; restarting it is not a persistent clipboard store.

## Architecture

- `src/main.rs`: source selection, format precedence, destination equality and
  latest-generation coordination.
- `src/payload.rs`: UTF-8/Latin-1 aliases, file-list normalization and copy/cut.
- `src/protocol.rs`: shared immutable buffers, commands, wake sockets and guards.
- `src/wayland.rs`: native data-control offers/sources and nonblocking pipe I/O.
- `src/x11.rs`: native XFixes, selection ownership, TARGETS and INCR state machines.

The protocol workers block on their display/wake descriptors and transfer
deadlines. Idle operation does not poll periodically. Display connection loss
is a process error so a service manager can restart it.

## Nix integration

This is a source input consumed with `callPackage`; it does not pin another
nixpkgs or require its own flake.

```nix
# flake.nix inputs
clipboard-bridge-src = {
  url = "github:sernseek/clipboard-bridge";
  flake = false;
};
```

```nix
clipboardBridge = pkgs.callPackage "${inputs.clipboard-bridge-src}/nix/package.nix" {
  source = inputs.clipboard-bridge-src;
};
```

Run `${clipboardBridge}/bin/clipboard-bridge` as a user service in the graphical
session. The package builds the Rust binary with `Cargo.lock`; it does not
install or replace system clipboard tools. Its build-source filter includes
only Cargo manifests and Rust source, not Git metadata or local dependencies.

Update the downstream pin deliberately:

```console
nix flake update clipboard-bridge-src
```

For a local working copy, stage new source files and pass
`--override-input clipboard-bridge-src git+file:///path/to/clipboard-bridge`
to the downstream build. The Git URL excludes untracked/ignored local files;
`path:` inputs instead snapshot the entire working directory before package
filtering and should not be used for a tree containing private ignored files.

## Development

```console
cargo test --locked
cargo clippy --all-targets -- -D warnings
cargo build --release --locked
```

Stop an existing instance before running `target/release/clipboard-bridge` by
hand. The executable connects directly to the display servers.

Unit tests cover URI encoding, copy/cut conflicts, invalid data and size bounds,
text encodings, generation/echo invariants, and INCR hint and accumulation rules.
Interoperability has also been exercised in isolated Sway/Xvfb sessions with
real clipboard clients: large bidirectional INCR, concurrent/early-closing
consumers, rich-text preservation, slow-source supersession, and Nautilus
file/folder copy and cut/move across the two displays.

Third-party Rust dependencies retain their respective upstream license terms.
