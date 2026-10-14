using System.IO.Pipes;
using System.Text;
using System.Text.Json;
using Inocrium.App.Models;
using Inocrium.App.Services;
using StreamJsonRpc;

// Unelevated, isolated pipe fixture; never contacts an installed service or disk.
using var lifetime = new CancellationTokenSource(TimeSpan.FromSeconds(90));
var pipeName = $"inocrium-gui-check-{Guid.NewGuid()}";
await using var pipe = new NamedPipeServerStream(pipeName, PipeDirection.InOut, 1, PipeTransmissionMode.Byte, PipeOptions.Asynchronous);
int inspectRequests = 0, prompts = 0, busyRequests = 0;
var serving = ServeAsync();
using var client = new RpcService(pipeName);
var connected = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
client.ConnectionChanged += (_, online) => { if (online) connected.TrySetResult(); };
client.Start();
try
{
    await connected.Task.WaitAsync(TimeSpan.FromSeconds(10));
    client.PasswordRequested = _ => Task.FromResult<string?>(++prompts == 1 ? "synthetic-wrong" : "synthetic-correct");
    var image = await client.CallAsync<ImageSummary>("images.inspect", new { path = "Türkçe ✓.irimg" });
    Check(prompts == 2 && inspectRequests == 3 && image.Path == "Türkçe ✓.irimg" && image.DefinitionId is null, "Password-required/bad-password retry or Unicode/nullable result failed.");
    client.PasswordRequested = _ => Task.FromResult<string?>(null);
    try { await client.CallAsync<ImageSummary>("images.inspect", new { path = image.Path }); throw new Exception("Expected password cancellation."); }
    catch (OperationCanceledException) { }
    Check(inspectRequests == 4, "Cancelled password request was replayed.");
    try { await client.CallAsync("jobs.cancel", new { id = Guid.NewGuid() }); throw new Exception("Expected busy error."); }
    catch (RemoteInvocationException error) when (error.ErrorCode == RpcErrorCodes.Busy) { }
    Check(busyRequests == 1, "Busy mutation was retried.");
    foreach (bool withData in new[] { false, true })
    foreach (int code in new[] { -32001, -32002, -32003, -32004, -32005, -32006, -32700, -32600, -32601, -32602, -32603 })
    {
        try { await client.CallAsync("contract.error", new { code, withData }); throw new Exception("Expected contract error."); }
        catch (RemoteInvocationException error) when (error.ErrorCode == code)
        {
            if (withData) Check(error.ErrorData is JsonElement data && data.GetProperty("detail").GetString() == "fixture", "Error data was reinterpreted.");
        }
    }
    await client.CallAsync("mounts.unmount", new { id = "all" });
    var definition = new Definition { Destination = @"C:\backup", Disks = [new(0, "test", null)], Retention = new("keepLast", 5), Mode = "forensic" };
    await client.CallAsync<Guid>("backup.start", RpcService.BackupParameters(definition));
    // Exceeds the old 30-second deadline without requiring a real iSCSI login.
    var mount = await client.CallAsync<Mount>("mounts.mount", new { path = image.Path, disk = (uint?)null, partition = (uint?)null, writable = false });
    Check(mount.DiskNumber is null && mount.Partition is null && mount.Letters.SequenceEqual(["Z"]), "Long mount result failed.");
    Console.WriteLine("PASS: all 11 Rust error codes with/without data, password retries/cancel, busy without replay, UTF-8, nullable DTOs, all-mount ID, flattened backup and >30s mount.");
}
finally
{
    client.Dispose();
    lifetime.Cancel();
    // The fixture was started above in this same console context and is cancelled
    // before awaiting cleanup; there is no UI synchronization context to deadlock.
#pragma warning disable VSTHRD003
    try { await serving; } catch (OperationCanceledException) { }
#pragma warning restore VSTHRD003
}

static void Check(bool condition, string message) { if (!condition) throw new Exception(message); }

async Task ServeAsync()
{
    await pipe.WaitForConnectionAsync(lifetime.Token);
    while (!lifetime.IsCancellationRequested)
    {
        using var header = new MemoryStream();
        var single = new byte[1];
        while (true)
        {
            if (await pipe.ReadAsync(single, lifetime.Token) == 0) return;
            header.WriteByte(single[0]);
            var bytes = header.GetBuffer();
            int n = (int)header.Length;
            if (n >= 4 && bytes[n - 4] == 13 && bytes[n - 3] == 10 && bytes[n - 2] == 13 && bytes[n - 1] == 10) break;
            Check(n < 8192, "Oversized request header.");
        }
        var lengthLine = Encoding.ASCII.GetString(header.ToArray()).Split("\r\n").Single(line => line.StartsWith("Content-Length:", StringComparison.OrdinalIgnoreCase));
        var body = new byte[int.Parse(lengthLine.Split(':')[1].Trim())];
        await pipe.ReadExactlyAsync(body, lifetime.Token);
        using var request = JsonDocument.Parse(body);
        var root = request.RootElement;
        var parameters = root.GetProperty("params");
        string method = root.GetProperty("method").GetString()!;
        object? result = null, error = null;
        switch (method)
        {
            case "service.info": result = new { version = "fixture", protocolVersion = 1, machine = "test" }; break;
            case "images.inspect":
                inspectRequests++;
                string? password = parameters.TryGetProperty("password", out var secret) ? secret.GetString() : null;
                if (password != "synthetic-correct") error = new { code = password is null ? -32004 : -32005, message = "Password rejected" };
                else result = new { path = parameters.GetProperty("path").GetString(), created = DateTimeOffset.UtcNow, comment = "", sizeOnDisk = ulong.MaxValue, encrypted = true, definitionId = (Guid?)null, disks = Array.Empty<object>(), incomplete = false };
                break;
            case "jobs.cancel": busyRequests++; error = new { code = -32003, message = "Busy" }; break;
            case "contract.error":
                error = parameters.GetProperty("withData").GetBoolean()
                    ? (object)new { code = parameters.GetProperty("code").GetInt32(), message = "Contract error", data = new { detail = "fixture" } }
                    : new { code = parameters.GetProperty("code").GetInt32(), message = "Contract error" };
                break;
            case "mounts.unmount": Check(parameters.GetProperty("id").GetString() == "all", "Unmount-all contract failed."); break;
            case "backup.start":
                Check(!parameters.TryGetProperty("definition", out _) && parameters.GetProperty("mode").GetString() == "forensic" && parameters.GetProperty("retention").GetProperty("kind").GetString() == "keepLast", "Flattened backup/enum contract failed.");
                result = Guid.NewGuid(); break;
            case "mounts.mount":
                await Task.Delay(TimeSpan.FromSeconds(31), lifetime.Token);
                result = new { id = Guid.NewGuid(), image = "Türkçe ✓.irimg", disk = 0, partition = (uint?)null, writable = false, diskNumber = (uint?)null, letters = new[] { "Z" } }; break;
            default: throw new Exception($"Unexpected method {method}.");
        }
        var response = error is null
            ? JsonSerializer.SerializeToUtf8Bytes(new { jsonrpc = "2.0", id = root.GetProperty("id"), result })
            : JsonSerializer.SerializeToUtf8Bytes(new { jsonrpc = "2.0", id = root.GetProperty("id"), error });
        await pipe.WriteAsync(Encoding.ASCII.GetBytes($"Content-Length: {response.Length}\r\n\r\n"), lifetime.Token);
        await pipe.WriteAsync(response, lifetime.Token);
        await pipe.FlushAsync(lifetime.Token);
    }
}
