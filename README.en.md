# NRL Pulse

A desktop client for amateur radio and emergency communications, implementing full-duplex voice dispatch over the NRL2 protocol. Supports Windows, macOS, and Linux.

![NRL Pulse](src-tauri/icons/icon-128.png)

---

## Features

### Voice Communication

- **Full-duplex**: Receive and transmit simultaneously without interference
- **PTT**: Tap to toggle transmit on/off; hold (320ms) to talk, release to stop
- **Keyboard hotkey**: Customizable PTT key (default: Space)
- **Floating PTT window**: Always-on-top mini window for one-hand operation
- **G.711 A-law** codec, 160-sample frames (20ms)
- **Auto resampling**: Uses device-native 8000 Hz when available, resamples otherwise

### Real-time Status

- Active speaker callsign displayed in real time
- RX / TX level meters and spectrum visualization (28 bands)
- Link quality: latency, jitter, packet loss, queue depth, uplink / downlink bitrate
- Connection state: connecting / connected / recovering / offline

### Dispatch Messages

- Send and receive text dispatch messages, Ctrl+Enter to send
- Message history (last 40 messages)

### Online Devices

- List of online stations in the current group
- Multi-group switching

### Devices & Configuration

- Auto-detects default audio input / output devices
- Adjustable jitter buffer
- AGC / noise suppression status
- Persistent local config (server, port, callsign, SSID, volume, PTT key)

### Platform Account

- Platform account login / logout
- Automatic session restore on launch
- Server list fetch and switching

### Other

- Chinese / English UI, switchable at runtime
- AT state sync (push local AT state to remote nodes)
- System log panel (device init, connection events, voice session records)
- Recording toggle

---

## Download

Download the binary for your platform from the [Releases](../../releases) page:

| Platform | File |
| --- | --- |
| Windows | `nrl-pulse-windows.exe` |
| Linux | `nrl-pulse-linux` |
| macOS Apple Silicon | `nrl-pulse-mac-arm` |
| macOS Intel | `nrl-pulse-mac-x64` |

---

## Quick Start

1. Download and run the binary for your platform
2. Click **Login** and enter your platform account credentials
3. Select a group, then click **Connect** to join a voice session
4. Press Space (or your configured hotkey) to start PTT transmission

---

## Protocol

Built on the **NRL2** voice dispatch protocol — UDP transport, G.711 A-law encoding, 20ms frame length.
