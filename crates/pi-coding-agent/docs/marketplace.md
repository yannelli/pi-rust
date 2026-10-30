# Extension marketplace

The marketplace contains native Rust extensions for pi-rust. Each release has a signed index and binary archives for supported platforms.

## Find an extension

```sh
pi extensions search
pi extensions search todo
pi extensions info todo
```

Search checks names, descriptions, and tags. Results show the release tag of each installed extension.

In interactive mode, `/extensions` shows the installed extensions and the marketplace. You can search, install, update, remove, enable, and disable extensions there. The [/extensions](commands.md#extensions) section gives the keys.

## Install an extension

```sh
pi install marketplace:todo
pi extensions install marketplace:todo
```

Both commands install the extension and add its source to `settings.json`. Restart pi or run `/reload` to load it.

To select a release, add its tag:

```sh
pi install marketplace:todo@v0.1.0
```

A source with a tag stays on that release. Install the source without a tag to use the latest release again.

Add `-l` to save the source in project settings. The project must be trusted. Marketplace package files and binaries stay in the agent directory.

## Update or remove an extension

```sh
pi update marketplace:todo
pi update --extensions
pi remove marketplace:todo
```

Updates select the latest release for sources without a tag. A plain `pi update` updates pi itself.

Removal deletes the generated package directory. It also deletes cached binaries that no installed marketplace package uses.

## Verification and storage

pi checks the Ed25519 signature over the exact index bytes. It then checks the schema, protocol, minimum pi version, and platform.
Before it extracts an archive, pi checks its SHA-256 digest and byte count against the signed index.

Supported targets:

- macOS on arm64 and x86-64.
- Linux on arm64 and x86-64, with musl binaries.
- Windows on x86-64, with MSVC binaries.

The default agent directory is `~/.pi/agent`. Set `PI_CODING_AGENT_DIR` to use another directory.

| Path under the agent directory | Contents |
| --- | --- |
| `marketplace/packages/<name>/package.json` | The generated manifest, with a `pi.native` command and arguments |
| `marketplace/bin/<tag>/<target>/` | Binaries shared by extensions from the same release |

The default index comes from releases of `yannelli/pi-rust-extensions` on GitHub.
Set `PI_MARKETPLACE_URL` to use another asset base URL. The public key and signature check stay the same.
With this override, pi reads `<base>/index.json` for the latest release and `<base>/<tag>/index.json` for a specific release.
The signature file is `index.json.sig` next to the index. Binary URLs come from the signed index.

With `PI_OFFLINE=1`, pi uses installed extensions without a download. Search and new installs need network access. In `/extensions`, the Marketplace tab shows `Marketplace unavailable:` with the error, and the Installed tab continues to work.
