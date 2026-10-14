# inocrium reflect GUI

Unpackaged WinUI 3 / .NET 8 client. Requires Windows 10 1809+ (Windows 11
recommended for Mica), .NET SDK 8+, a Windows SDK and WinUI build tools. The
Windows App SDK 1.6 runtime is included with the application. NuGet access is
required for the first restore. Build from the repository root:

```powershell
dotnet build gui/Inocrium.sln -c Release
```

The solution defaults to x64. For ARM64, build the project with
`dotnet build gui/Inocrium.App/Inocrium.App.csproj -c Release -p:Platform=ARM64`.
Each project uses its own bin/obj. Launch the generated Inocrium.App.exe in
`gui/Inocrium.App/bin/x64/Release/net8.0-windows10.0.19041.0/win-x64/` and approve
the UAC prompt. The manifest requiresAdministrator because the service pipe only
allows elevated Administrators and SYSTEM. Install/start inocrium-service
separately; this GUI does not install or elevate the service for you.

The MSIX BuildTools NuGet package supplies portable PRI tasks for `dotnet build`;
Visual Studio-specific MSBuild assembly paths are not required. This is a JIT
.NET 8 app, not a Native AOT build. The .NET 8 Desktop Runtime is required to run
the normal build output; the Windows App SDK runtime is self-contained.

The GUI uses one StreamJsonRpc connection over NamedPipeClientStream with
HeaderDelimitedMessageHandler and SystemTextJsonFormatter (camelCase). It checks
protocol version 1, reconnects every two seconds when disconnected, and refreshes
lists after reconnect. It never replays a request after a timeout/disconnect.
Mount, unmount and viBoot requests allow 180 seconds; other requests allow 30.
An InfoBar
shows unavailable service/access errors. Long operations appear in Activity,
with notifications updating progress/rate/ETA and state. Window close cancels
connection retries and disposes the RPC transport.

Pages: Disks (proportional partition bars and backup dialog), Backup definitions
(disk/partition selection, UNC credentials, encryption, retention and schedules),
Schedule, Existing images (inspect, mount, verify, restore, VHDX export and viBoot),
Mounts, Activity/History with log viewer, and Settings/network credentials/About.
Restore requires a selected target and typed `RESTORE <disk number>` confirmation.
The service must still independently enforce destructive-operation safety.
PASSWORD_REQUIRED (-32004) and BAD_PASSWORD (-32005) prompt for a password and
retry only the rejected request. Cancel stops the action. Passwords are not
persisted or logged by the GUI. The service can retain saved-definition and
network-credential secrets in its protected store. `definitions.run` accepts
only an ID, so a password error falls back to `definitions.get` and the flattened
`backup.start` request with a transient password.
Folder and save pickers are initialized with the WinUI window handle.

Rust error codes are preserved as `RemoteInvocationException.ErrorCode`, including
BUSY (-32003), which overlaps a StreamJsonRpc extension code. viBoot reports a
friendly Hyper-V requirement when the service returns MethodNotFound (-32601).

`ProtocolChecks` is included in the solution and runs in CI without elevation or
an installed service. It links the actual client/DTO sources and uses a unique
local fixture pipe. Run `dotnet run --project gui/ProtocolChecks -c Release` to
check all eleven error codes, password retries/cancellation, UTF-8, nullable DTOs,
backup/unmount parameters and a mount taking more than 30 seconds. The Rust IPC
cross-language fixture additionally covers notifications and service restart.

Architecture: `Models/Protocol.cs` mirrors the Rust DTOs; `Services/RpcService.cs`
owns reconnect/RPC/notification handling; CommunityToolkit viewmodels hold state
and commands; MainWindow and its partial page/dialog files construct WinUI views.
Microsoft.Extensions.DependencyInjection supplies the RPC service and shell
viewmodel. See [the IPC contract](../crates/ipc/PROTOCOL.md) for all methods,
field shapes, errors and security requirements.

Licensed GPL-3.0-only. Full license is in the repository root LICENSE.

The MSI installs the full `dotnet publish --self-contained true` output, including
`Inocrium.App.pri`, compiled XAML resources and both .NET/Windows App SDK runtimes.
Some XBF resources are embedded in PRI files; harvesting only EXE/DLL files is
insufficient. `installer/build.ps1` validates the payload before WiX harvesting.
The CLI is always installed; opt in to system PATH with
`msiexec /i inocrium-reflect-0.1.0-x64.msi ADDLOCAL=ALL`.
