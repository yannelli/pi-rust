# Install

## Install script

Linux and macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/yannelli/pi-rust/main/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/yannelli/pi-rust/main/install.ps1 | iex
```

The script does these steps:

1. It downloads the latest release for your platform.
2. It compares the archive with `SHA256SUMS`.
3. It installs `pi` in `~/.local/bin`. On Windows, it installs `pi` in `%LOCALAPPDATA%\Programs\pi` and adds that folder to your user `Path`.

The scripts do not use sudo and do not send telemetry. `pi update --self` updates an installation from these scripts.

### Script options

To change the installation, set these environment variables:

| Variable | Effect |
| --- | --- |
| `PI_VERSION` | Install this release, for example `0.5.3`, in place of the latest release. |
| `PI_INSTALL_DIR` | Install `pi` in this directory. Do not use `~/.cargo/bin`, because `pi update --self` treats a binary there as a cargo installation. |
| `PI_RELEASES_URL` | Download from a mirror of `https://github.com/yannelli/pi-rust/releases`. The mirror must serve `latest/download/SHA256SUMS` and `download/<tag>/<file>`. |

Example:

```sh
curl -fsSL https://raw.githubusercontent.com/yannelli/pi-rust/main/install.sh | PI_VERSION=0.5.3 sh
```

In PowerShell, set `$env:PI_VERSION = "0.5.3"` before `irm`.

## Manual install

Each [release](https://github.com/yannelli/pi-rust/releases) has one archive for each platform and a `SHA256SUMS` file. Each archive contains a `pi-vX.Y.Z-<target>/` folder with these files:

- `pi` or `pi.exe`
- `README.md` and `CHANGELOG.md`
- `LICENSE`, `LICENSE-pi-acp`, `LICENSE-node`, `LICENSE-v8`, `LICENSE-mcp-typescript-sdk`, `LICENSE-chalk`, `LICENSE-proper-lockfile`, `LICENSE-signal-exit`, and `THIRD-PARTY-LICENSES.md`

| Platform | Target |
| --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-musl` |
| Linux arm64 | `aarch64-unknown-linux-musl` |
| Linux armv7 (32-bit) | `armv7-unknown-linux-musleabihf` |
| macOS Apple silicon | `aarch64-apple-darwin` |
| macOS Intel | `x86_64-apple-darwin` |
| Windows x86_64 | `x86_64-pc-windows-msvc` |
| Windows arm64 | `aarch64-pc-windows-msvc` |

### Linux and macOS

The Linux binaries are static, so they run on all distributions, including Termux.

```sh
target=x86_64-unknown-linux-musl   # pick yours from the table
tag=$(curl -fsSL https://api.github.com/repos/yannelli/pi-rust/releases/latest | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p')
curl -fsSLO "https://github.com/yannelli/pi-rust/releases/download/$tag/pi-$tag-$target.tar.gz"
curl -fsSLO "https://github.com/yannelli/pi-rust/releases/download/$tag/SHA256SUMS"
grep " pi-$tag-$target.tar.gz$" SHA256SUMS | sha256sum -c -   # macOS: shasum -a 256 -c -
tar -xzf "pi-$tag-$target.tar.gz"
mkdir -p ~/.local/bin && install -m 755 "pi-$tag-$target/pi" ~/.local/bin/pi   # ~/.local/bin must be on PATH
pi --version
```

If macOS stops `pi` after a browser download, run `xattr -d com.apple.quarantine pi`.

### Windows

```powershell
$target = "x86_64-pc-windows-msvc"   # or aarch64-pc-windows-msvc
$tag = (Invoke-RestMethod https://api.github.com/repos/yannelli/pi-rust/releases/latest).tag_name
Invoke-WebRequest "https://github.com/yannelli/pi-rust/releases/download/$tag/pi-$tag-$target.zip" -OutFile pi.zip
Expand-Archive pi.zip -DestinationPath "$env:LOCALAPPDATA\pi" -Force
$bin = "$env:LOCALAPPDATA\pi\pi-$tag-$target"
[Environment]::SetEnvironmentVariable("Path", "$([Environment]::GetEnvironmentVariable('Path', 'User'));$bin", "User")
```

Then open a new terminal. The `Path` change applies only to new terminals.

### GitHub CLI

To download the latest archive with the GitHub CLI, run this command:

```sh
gh release download --repo yannelli/pi-rust --pattern '*<target>*'
```

### Verify the build provenance

Each release file has a signed build provenance attestation. To make sure that a file came from the Release workflow of this repository, run this command:

```sh
gh attestation verify pi-$tag-$target.tar.gz --repo yannelli/pi-rust
```

On Windows, use the `.zip` file name.

## Update

- `pi update --self` replaces a release installation with the latest release.
- `pi update --models` updates the model catalogs.

## Build from source

You need Rust 1.85 or newer.

```sh
cargo install --path crates/pi-coding-agent
```
