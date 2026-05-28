# FreeDesktop Desktop Notifications Specification — Summary

> Source: [specifications.freedesktop.org/notification/1.3](https://specifications.freedesktop.org/notification/1.3)
> Version 1.3, 18 August 2024
> Authors: Mike Hearn, Christian Hammond, William Jon McCann

## Overview

A D-BUS–based standard for desktop notification services. Applications send passive popups that notify the user asynchronously. It explicitly does **not** cover modal dialogs, window-manager decorations, or window-list annotations.

Use cases: chat messages, alarms, file-transfer completion, new mail, low disk/battery warnings.

---

## Architecture

- A single **session-scoped service** owns the bus name `org.freedesktop.Notifications` on the D-BUS session bus.
- The object path is `/org/freedesktop/Notifications`; the interface is `org.freedesktop.Notifications`.
- Autostart via the bus daemon is optional; clients must not assume the service is always present.

### Notification components

| Component | Description |
|---|---|
| Application Name | Optional formal name (e.g. `"FredApp E-Mail Client"`). |
| Replaces ID | Optional ID of an existing notification to atomically replace. |
| Notification Icon | `app_icon` parameter (see Icons & Images). |
| Summary | Single-line overview; recommended ≤ 40 chars, UTF-8. |
| Body | Multi-line body with optional markup; UTF-8. |
| Actions | List of `(key, localized_label)` pairs; `"default"` key for click action. |
| Hints | `a{sv}` dict of extra data (see Hints table). |
| Expiration Timeout | ms until auto-close. `-1` = server default, `0` = never. |

Each notification gets a **unique uint32 ID** (never 0). IDs are not recycled unless `MAXINT` is exceeded.

---

## D-BUS Protocol

### Methods

#### `GetCapabilities` → `as`
Returns an array of capability strings the server supports:

| Capability | Meaning |
|---|---|
| `action-icons` | Icons instead of text for action buttons. |
| `actions` | Server will present actions to the user. |
| `body` | Supports body text. |
| `body-hyperlinks` | Supports `<a>` hyperlinks. |
| `body-images` | Supports `<img>` in body. |
| `body-markup` | Supports markup tags (strip them if absent). |
| `icon-multi` | Renders all frames of a multi-frame image. |
| `icon-static` | Renders exactly 1 frame of an image array. Mutually exclusive with `icon-multi`. |
| `persistence` | Retains notifications until acknowledged/removed. |
| `sound` | Supports `sound-file` and `suppress-sound` hints. |

Vendor extensions: prefix with `x-vendor` (e.g. `x-gnome-foo-cap`).

#### `Notify (app_name, replaces_id, app_icon, summary, body, actions, hints, expire_timeout)` → `UINT32`
Sends a notification. Returns the new (or replacement) ID.

#### `CloseNotification (id)`
Force-close a notification by ID. Emits `NotificationClosed`.

#### `GetServerInformation ()` → `(name, vendor, version, spec_version)`
Returns server metadata.

### Signals

| Signal | Parameters | Description |
|---|---|---|
| `NotificationClosed` | `id: UINT32`, `reason: UINT32` | 1=expired, 2=dismissed by user, 3=CloseNotification call, 4=reserved. ID is invalidated *before* the signal. |
| `ActionInvoked` | `id: UINT32`, `action_key: STRING` | User invoked a notification action. Not all servers emit this. |
| `ActivationToken` | `id: UINT32`, `token: STRING` | Optional; emitted *before* `ActionInvoked`. Carries an X11 startup ID or Wayland xdg-activation token. |

---

## Markup

Body text may contain a **small XML-based subset of HTML**:

| Tag | Meaning |
|---|---|
| `<b>…</b>` | Bold |
| `<i>…</i>` | Italic |
| `<u>…</u>` | Underline |
| `<a href="…">…</a>` | Hyperlink (standard blue underline) |
| `<img src="…" alt="…"/>` | Image (local `file://` only; max 200×100 px) |

Servers that don't support markup must strip tags. No full HTML/CSS/XSLT — notifications are not web browsers.

---

## Icons & Images

### Image/icon priority (single-display servers)

1. `image-data` hint (raw pixel data, `(iiibiiay)` struct)
2. `image-path` hint (URI or themed icon name)
3. `app_icon` parameter
4. `icon_data` (deprecated, compat only)

### Raw image format (`image-data` / `icon_data`)

A D-Bus struct of signature `(iiibiiay)` matching gdk-pixbuf:

| Index | Field | Description |
|---|---|---|
| 0 | `width` (i) | Pixels |
| 1 | `height` (i) | Pixels |
| 2 | `rowstride` (i) | Bytes between row starts |
| 3 | `has_alpha` (b) | Whether alpha channel is present |
| 4 | `bits_per_sample` (i) | Must be 8 |
| 5 | `channels` (i) | 4 if has_alpha, else 3 |
| 6 | `data` (ay) | Raw pixel bytes, RGB order |

### Path-based images

`app_icon` and `image-path` accept:
- A `file://` URI (only supported scheme)
- A name from a freedesktop.org–compliant icon theme (not a GTK+ stock ID)

---

## Categories

Optional `class.specific` type indicator passed via the `category` hint. Servers may use them for grouping or display.

| Category | Description |
|---|---|
| `call` | Generic audio/video call |
| `call.ended` / `call.incoming` / `call.unanswered` | Call lifecycle |
| `device` / `device.added` / `device.error` / `device.removed` | Hardware devices |
| `email` / `email.arrived` / `email.bounced` | Email events |
| `im` / `im.error` / `im.received` | Instant messages |
| `network` / `network.connected` / `network.disconnected` / `network.error` | Network state |
| `presence` / `presence.offline` / `presence.online` | User presence |
| `transfer` / `transfer.complete` / `transfer.error` | File transfers/downloads |

Vendor categories: `x-vendor.class.name` form.

---

## Urgency Levels

Passed via the `urgency` hint (BYTE):

| Value | Level | Behaviour |
|---|---|---|
| 0 | Low | Server chooses display and expiration. |
| 1 | Normal | Default. Server chooses display with sane timeout. |
| 2 | Critical | Must not auto-expire — user must dismiss. |

---

## Hints

`a{sv}` dictionary passed to `Notify`. Neither clients nor servers are required to support any hint. Unknown hints must be silently ignored.

| Hint | Type | Description | Spec |
|---|---|---|---|
| `action-icons` | BOOLEAN | Interpret action keys as icon names (needs `action-icons` cap). | ≥ 1.2 |
| `category` | STRING | Notification type (see Categories). | — |
| `desktop-entry` | STRING | `.desktop` file basename (e.g. `"rhythmbox"` for `rhythmbox.desktop`). | — |
| `image-data` | (iiibiiay) | Raw image (gdk-pixbuf struct). | ≥ 1.2 |
| `image_data` | (iiibiiay) | **Deprecated** — use `image-data`. | 1.1 |
| `image-path` | STRING | Image URI or themed icon name. | ≥ 1.2 |
| `image_path` | STRING | **Deprecated** — use `image-path`. | 1.1 |
| `icon_data` | (iiibiiay) | **Deprecated** — use `image-data`. | < 1.1 |
| `resident` | BOOLEAN | Don't auto-remove after action invocation (needs `persistence` cap). | ≥ 1.2 |
| `sound-file` | STRING | Path to sound file to play. | — |
| `sound-name` | STRING | Themed sound name from [sound-naming-spec](http://0pointer.de/public/sound-naming-spec.html) (e.g. `"message-new-instant"`). | — |
| `suppress-sound` | BOOLEAN | Suppress server-side sound (client plays its own). | — |
| `transient` | BOOLEAN | Bypass persistence capability. | ≥ 1.2 |
| `x` | INT32 | Screen X position to point at (requires `y`). | — |
| `y` | INT32 | Screen Y position to point at (requires `x`). | — |
| `urgency` | BYTE | 0=low, 1=normal, 2=critical. | — |

---

## Backwards Compatibility

- Always call `GetCapabilities` before relying on optional features.
- If the server lacks `actions`, fall back to a non-focus-stealing message box.
- Strip markup if `body-markup` is not advertised.
- Ignore unknown hints and capabilities.
- Vendor extensions must use `x-vendor` prefix.

---

## Key Implementations

Well-known notification daemons implementing this spec:
- **GNOME**: `gnome-shell` (built-in), `notification-daemon`
- **KDE**: `plasma-workspace` (built-in)
- **Xfce**: `xfce4-notifyd`
- **Standalone**: `dunst`, `mako`, `wired-notify`
- **Libraries**: `libnotify` (C), `notify-rust` (Rust), `python-notify2` (Python)
