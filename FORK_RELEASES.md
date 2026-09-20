# Download and install

These are unofficial builds of [gu1p/zed](https://github.com/gu1p/zed), including
collapsible test-file groups in the Project Panel.

| Computer | Download |
| --- | --- |
| Mac with Apple Silicon (M1 or newer) | `Zed-aarch64.dmg` |
| Intel Mac | `Zed-x86_64.dmg` |
| Linux x86-64, Ubuntu 24.04 or compatible | `zed-linux-x86_64.tar.gz` |
| Linux ARM64, Ubuntu 24.04 or compatible | `zed-linux-aarch64.tar.gz` |

On macOS, open the DMG and drag **Zed.app** into **Applications**. These builds use
ad-hoc signing and are **not notarized by Apple**. If macOS blocks the first launch,
use **System Settings > Privacy & Security > Open Anyway** after attempting to
open Zed. If macOS instead reports the downloaded app as damaged, remove its
quarantine flag after copying it to Applications:

```sh
xattr -dr com.apple.quarantine /Applications/Zed.app
```

On Linux, extract the archive and run `zed.app/bin/zed`. The archive includes the
desktop entry and icons under `zed.app/share/`.

Enable the feature in Zed's settings:

```json
{
  "project_panel": {
    "group_test_files": true
  }
}
```

Upstream automatic updates are disabled in these builds. Download newer versions
from [this fork's Releases page](https://github.com/gu1p/zed/releases/latest).
`SHA256SUMS` contains checksums for all packages. The remote-server archives are
optional; you do not need them to install the desktop app.

# Build automation

The **Build fork releases** workflow builds native macOS and Linux packages for
both architectures on standard GitHub-hosted runners. It runs on every push to
`main`, on tags starting with `fork-v`, and through **Actions > Build fork
releases > Run workflow**. No Apple certificate, notarization account, or other
repository secret is required; publication uses GitHub's built-in token.

A release becomes public after all four builds and package checks succeed.
Branch/manual builds use `fork-build-<run number>` tags; tagged builds use the
pushed tag. Failed builds leave any successful packages available as Actions
artifacts for seven days. Re-running a release job replaces the assets for that
same release instead of creating a duplicate.
