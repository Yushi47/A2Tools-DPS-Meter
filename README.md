# A2Tools DPS Meter

[![License](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](LICENSE)
[![GitHub Issues](https://img.shields.io/github/issues/taengu/A2Tools-DPS-Meter)](https://github.com/taengu/A2Tools-DPS-Meter/issues)
[![GitHub Pull Requests](https://img.shields.io/github/issues-pr/taengu/A2Tools-DPS-Meter)](https://github.com/taengu/A2Tools-DPS-Meter/pulls)

Real-time DPS meter overlay for AION 2. Captures game network packets to display damage, skills, and combat statistics.

**[Download Latest Release](https://github.com/taengu/A2Tools-DPS-Meter/releases)** | **[A2Tools.app](https://a2tools.app)**

[한국어](README_KO.md) | [简体中文](README_ZH.md) | [繁體中文](README_ZH-TW.md)

## Features

- Real-time DPS tracking with per-player breakdown
- Skill-level damage analysis with crit, back attack, parry, double, and perfect rates
- DOT (damage over time) tracking
- Summon damage merged with owner
- Multiple target selection modes (Boss, Last Hit, All Targets, Train)
- DPS chart and timeline visualization
- Battle history with auto-save for boss fights
- Ping monitoring
- Multi-language support (English, Korean, Chinese Traditional/Simplified)
- Always-on-top transparent overlay
- Themes and customization

## Requirements

- **Windows 10/11** (x86_64)
- **[Npcap](https://npcap.com)** — required for packet capture
  - During Npcap installation, check **"Install Npcap in WinPcap API-compatible Mode"**
- **Administrator privileges** — required for raw packet capture

## Installation

1. Install [Npcap](https://npcap.com) with WinPcap API-compatible mode enabled
2. Download the latest MSI installer from [Releases](https://github.com/taengu/A2Tools-DPS-Meter/releases)
3. Run the installer
4. Launch A2Tools DPS Meter (run as Administrator)

## Building from Source

### Prerequisites

- [Rust](https://rustup.rs/) (latest stable)
- [Node.js](https://nodejs.org/) (v18+)
- [Npcap](https://npcap.com) installed

### Build

```bash
npm install
npm run tauri build
```

The MSI installer will be at `src-tauri/target/release/bundle/msi/`.

### Development

```bash
npm run tauri dev
```

## FAQ

**Q: The meter shows "Detecting AION2 connection..."**
A: Make sure AION 2 is running and the app has administrator privileges. If using a VPN or ping reducer, the app will detect the loopback adapter automatically.

**Q: My name doesn't appear on the meter**
A: Enter your character name and actor ID in Settings. The name is auto-detected from the AION 2 window title.

**Q: Npcap is installed but capture doesn't work**
A: Reinstall Npcap and ensure "WinPcap API-compatible Mode" is checked during installation.

## Community

- [Discord](https://discord.gg/Aion2Global)
- [A2Tools.app](https://a2tools.app)

## Support

Say thanks and fund new cool projects & features!

- <img src="wechat.png" width="150">
- ☕ [Buy me a Coffee](https://ko-fi.com/hiddencube)
- ☕ [在爱发电支持我](https://afdian.com/a/hiddencube)
- 🅿️ [Send with PayPal](https://www.paypal.me/taengoo)
- 🎁 [Donate with Crypto](https://nowpayments.io/donation/thehiddencube)
- **BTC**: `1GexKhgVZPYRqpfCKydXLoNUXRRRUoAUwT`
- **ETH**: `0x38F0bc371A563A24eCa6034cFf77eB6173c7e3e7`
- **USDC**: `0xA9571Fc95666350f6DFFB8Fb80ee27eE7db46b56`

## License

[GPL-3.0](LICENSE)
