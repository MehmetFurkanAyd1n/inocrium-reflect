# Windows MSI installer

Prerequisites: Windows, PowerShell, .NET 8 SDK or newer, Rust stable with the MSVC
toolchain, and Visual Studio 2022/Build Tools with C++ build tools, Windows SDK,
and the WinUI/UWP build components (including PRI/XAML tasks). Use a Visual Studio
Developer PowerShell with `msbuild` on PATH. NuGet access is needed to restore the
pinned WiX Toolset SDK and UI/Util extensions (all version 5.0.2); no global WiX or
heat installation is needed. Installing the MSI requires administrator privileges.

From the repository root:

```powershell
rustup target add x86_64-pc-windows-msvc
.\installer\build.ps1 -Configuration Release -Platform x64 -Version 0.1.4
```

ARM64 uses the same authoring with a platform property. Install the ARM64 MSVC
compiler/linker and Windows SDK libraries, then:

```powershell
rustup target add aarch64-pc-windows-msvc
.\installer\build.ps1 -Platform ARM64 -Version 0.1.4
```

The script builds the Rust service and CLI in release mode for the requested
architecture, publishes the unpackaged GUI with self-contained .NET/Windows App
SDK to `installer/out/staging/<Platform>/gui`, copies the full GPL license into
that payload, and builds WiX. `Configuration` selects the GUI and MSI configuration;
Rust is always built with `--release`. Outputs are
`installer/out/<Platform>/inocrium-reflect-<Version>-<Platform>.msi`.
Version must be three numeric MSI fields (major/minor <= 255, patch <= 65535).
The script respects `CARGO_TARGET_DIR` (default: `target`) and harvests binaries
from its `<Rust target>/release` directory.
The unpackaged GUI resource index is `Inocrium.App.pri`.

WiX validation remains enabled except for ICE03. Windows App SDK 1.6's XAML DLLs
contain language lists longer than ICE03's column limit, and its MUI files use
LCIDs 1152, 1153 and 1169 missing from ICE03's older language catalog. Their
vendor binaries and detected metadata are preserved; other ICE checks still run.

For existing payloads, build WiX directly:

```powershell
dotnet build installer/Inocrium.Installer.wixproj -c Release -p:Platform=x64 `
  -p:ProductVersion=0.1.4 '-p:RustOutDir=C:\payload\rust' `
  '-p:GuiPublishDir=C:\payload\gui'
```

`ProductVersion` defaults to `0.1.4`; `RustOutDir` defaults to `target/release`;
`GuiPublishDir` defaults to the staging path above. Include `LICENSE` in a manually
prepared GUI payload. The GUI project has no explicit `AssemblyName`, so its
project name yields `Inocrium.App.exe`. The Rust binaries are `inocrium.exe` and
`inocrium-service.exe`.

The per-machine MSI installs to `ProgramFiles64Folder\inocrium reflect`, optionally
appends the install directory to the system PATH (`ADDLOCAL=ALL`), creates a Start-menu shortcut, and
registers `InocriumReflect` as an automatic LocalSystem service. It starts/stops
the service during installation/removal and configures restart after failures
(60-second delay, reset after one day). The service must enter Windows service
mode when launched without arguments. No `.ico` currently exists, so the shortcut
has no explicit icon. The shortcut uses an HKCU registry key path for the installing
user; shortcuts for other users can be managed through MSI repair.

Major upgrades reject downgrades. The UpgradeCode is permanent; x64 and ARM64 are
alternative architectures of this product, not supported side-by-side installations.
Uninstall removes installed binaries and the MSI-owned PATH entry, but intentionally
preserves `%ProgramData%\inocrium` user definitions/configuration. Neither build nor
authoring validation proves installation/service behavior; test the MSI on a clean
Windows machine with the matching architecture before release.
