# NRL Pulse

A desktop client for amateur radio and emergency communications. It supports both **NRL2** (UDP / G.711 / Opus) and **FMO** (MQTT / APRS server discovery / SAS certificate authentication / Opus / IMA ADPCM) protocols, with full-duplex voice dispatch. Available on Windows, macOS, and Linux.

![NRL Pulse](src-tauri/icons/icon-128.png)

---

## Features

### Voice Communication

- **Dual-protocol support**: switch between NRL and FMO in Settings
  - **NRL**: UDP transport, G.711 A-law (type 1) or **Opus (type 8)** voice codec
    - NRL Opus spec: 16 kHz / mono / 20 ms (320-sample) frames / 32–40 kbps VBR / OPUS_APPLICATION_VOIP / complexity 10
  - **FMO**: MQTT connection (`FMO/RAW` voice), with **Opus (SILK NB 8 kHz)** and **IMA ADPCM** selectable
- **Full-duplex**: receive and transmit simultaneously without interference
- **PTT transmit**: tap to toggle TX state, hold (320 ms) to keep transmitting, release to stop
- **Keyboard hotkey**: customizable PTT trigger key (default: Space)
- **Floating PTT window**: standalone always-on-top mini window for one-hand operation
- **Independent mute button**: mute received audio independently (in FMO mode, disable decoded playback)
- **Automatic resampling**: prefer the device's native 8000 Hz, automatically resample when unsupported

### FMO Mode

- **APRS-IS server discovery**: connect to rotate.aprs2.net and parse FMO-V4 STATION beacons in real time to maintain a server list
- **Certificate import**: upload `cert_user.json`, `cert_int.json`, `cert_root.json`, and `cert_devicekey.json` to set the current identity (certificates must be backed up from FMO with a backup tool)
- **Auto-activate certificate**: bind this device's MAC and fetch/activate the identity from the certificate server, provided the MAC has already been registered on `hamptt.com`
- **SAS authentication**: automatically build MQTT credentials from Ed25519 / CBOR certificate fingerprints
- **Server list / favorites**: sort by online count, star a server for one-click switching

### Real-time Status

- Real-time active speaker callsign display
- RX / TX level meters and spectrum visualization (28 bands)
- Network quality metrics: latency, jitter, packet loss, queued frames, uplink / downlink bitrate
- Connection state: connecting / connected / recovering / offline

### Dispatch Messages

- Send and receive text dispatch messages, Ctrl+Enter to send
- Message history (last 40 messages)

### Online Devices

- List of online stations in the current group
- Multi-group switching

### Devices & Configuration

- Auto-detect default audio input / output devices
- Adjustable jitter buffer
- AGC / noise suppression / AEC (Acoustic Echo Cancellation on Windows WASAPI / macOS AUVoiceIO) status
- Persistent local config (protocol, server, port, callsign, SSID, volume, PTT key, voice codec, serial port, recording path)

### Platform Account

- Platform account login / logout
- HAM ID long-term token login
- Automatic session restore on launch
- Server list fetch and switching

### Voice Bridge

- Bidirectional voice bridge between NRL and FMO
- Mode cycle: Off → FMO→NRL → NRL→FMO → Both
- Real-time bridge direction and transmit state indication

### Serial Tunnel

- Physical serial port ↔ NRL/FMO voice link data bridge
- Configurable baud rate, data bits, parity, stop bits, flow control
- Real-time running status and RX/TX byte counters

### Local Recording

- One-click voice recording toggle
- Recordings saved to a user-specified directory

### Room Monitor

- Standalone monitor window ("Monitor" button in the top toolbar)
- View all visible rooms on the current NRL server via WebSocket
- Selectively listen to room voice mix (G.711 A-law, 8 kHz)
- Logged-in users can "Join" a target room; the main window group syncs automatically
- See [docs/room-monitor.md](docs/room-monitor.md) for more details

### FMO Advanced

- **QSO Call**: initiate a single-station voice call via APRS signaling, with incoming call popup / auto-answer and QSO log
- **Server Broadcast**: broadcast your own FMO server information to the network via APRS STATION beacons
- **Personal Beacon (BEACON)**: periodically send APRS beacons, configurable SSID, rig, antenna, frequency, custom message, etc.

### Other

- Chinese / English bilingual UI, switchable at runtime
- AT state sync (push local AT state to remote nodes)
- System log panel (device init, connection events, voice session records)
- Windows installer supports automatic updates

---

## Download

Download the binary for your platform from the [Releases](../../releases) page:

| Platform | File |
| --- | --- |
| Windows (recommended, auto-update) | `nrl-pulse-windows-setup.exe` |
| Windows portable | `nrl-pulse-windows.exe` |
| Linux | `nrl-pulse-linux` |
| macOS Apple Silicon | `nrl-pulse-mac-arm` |
| macOS Intel | `nrl-pulse-mac-x64` |

---

## Quick Start

### NRL Mode

1. On Windows, install `nrl-pulse-windows-setup.exe`; the portable build cannot reliably replace itself in place
2. Click **Login** and enter your platform account credentials, or use a HAM ID token
3. Select a group, then click **Connect** to join a voice session
4. Press Space (or your configured hotkey) to start PTT transmission
5. Click the top **Monitor** button to open the room monitor window and view or join other rooms

### FMO Mode

1. In Settings, switch the protocol to **FMO**
2. Import the certificate JSON files (cert_user / cert_int / cert_devicekey, etc.) or click **Auto Activate** to fetch certificates by binding your MAC
3. Connect APRS to discover FMO servers, then select one (or choose from favorites)
4. Click **Connect** to establish the MQTT session, then press Space to PTT transmit
5. Configure QSO, server broadcast, and personal beacon in Settings

---

## Protocol

### NRL2

Based on the **NRL2** voice dispatch protocol — UDP transport, 20 ms frame length.
- type 1: G.711 A-law voice (8 kHz)
- type 8: **Opus** voice (16 kHz / 20 ms / VOIP / complexity 10)
- type 2: heartbeat
- type 5: text
- type 11: AT
- type 12: serial tunnel

### FMO

Based on the **FMO-V4** protocol:
- Discovery: APRS-IS (rotate.aprs2.net:10152) FMO-V4 STATION broadcasts
- Authentication: SAS (Ed25519 signature + CBOR certificate fingerprint → MQTT password)
- Voice: MQTT `FMO/RAW`, Opus (SILK NB 8 kHz) or IMA ADPCM (80 ms blocks, 320 B)
- Telemetry: `FMO/TELE` · Server info: `FMO/SERVER_INFO`
