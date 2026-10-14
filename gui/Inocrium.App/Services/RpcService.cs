using System.IO.Pipes;
using System.Text.Json;
using Inocrium.App.Models;
using StreamJsonRpc;

namespace Inocrium.App.Services;
public sealed class RpcService(string pipeName = "inocrium-reflect") : IDisposable
{
    private readonly CancellationTokenSource lifetime = new();
    private readonly object gate = new();
    private JsonRpc? rpc;
    private Task? reconnect;
    public event Action<string, bool>? ConnectionChanged;
    public event Action<Guid, Progress>? ProgressChanged;
    public event Action<Job>? JobChanged;
    public event Action? MountsChanged;
    public event Action? DefinitionsChanged;
    public void Start() { lock (gate) reconnect ??= ReconnectAsync(); }
    public bool IsConnected { get { lock (gate) return rpc is { IsDisposed: false }; } }
    public async Task<T> CallAsync<T>(string method, object? parameters = null)
    {
        JsonRpc connection;
        lock (gate) connection = rpc ?? throw new IOException("The service is not connected.");
        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(lifetime.Token);
        timeout.CancelAfter(TimeSpan.FromSeconds(30));
        return await connection.InvokeWithParameterObjectAsync<T>(method, parameters ?? new { }, timeout.Token);
    }
    public async Task CallAsync(string method, object? parameters = null) => await CallAsync<JsonElement>(method, parameters);
    private async Task ReconnectAsync()
    {
        while (!lifetime.IsCancellationRequested)
        {
            NamedPipeClientStream? pipe = null;
            JsonRpc? connection = null;
            try
            {
                pipe = new NamedPipeClientStream(".", pipeName, PipeDirection.InOut, PipeOptions.Asynchronous, System.Security.Principal.TokenImpersonationLevel.Impersonation);
                await pipe.ConnectAsync(2000, lifetime.Token);
                var formatter = new SystemTextJsonFormatter();
                formatter.JsonSerializerOptions.PropertyNamingPolicy = JsonNamingPolicy.CamelCase;
                formatter.JsonSerializerOptions.PropertyNameCaseInsensitive = true;
                var handler = new HeaderDelimitedMessageHandler(pipe, pipe, formatter);
                connection = new JsonRpc(handler, new Notifications(this));
                connection.StartListening();
                lock (gate) rpc = connection;
                var info = await CallAsync<ServiceInfo>("service.info");
                if (info.ProtocolVersion != 1) throw new IOException($"Unsupported service protocol {info.ProtocolVersion}; this GUI requires 1.");
                ConnectionChanged?.Invoke($"Connected to {info.Machine} · service {info.Version}", true);
                await connection.Completion.WaitAsync(lifetime.Token);
                ConnectionChanged?.Invoke("Service disconnected. Reconnecting…", false);
            }
            catch (OperationCanceledException) when (lifetime.IsCancellationRequested) { break; }
            catch (UnauthorizedAccessException) { ConnectionChanged?.Invoke("Access denied. Run inocrium reflect as administrator.", false); }
            catch (Exception error) { ConnectionChanged?.Invoke(error is TimeoutException ? "Service is not running. Start the inocrium-service Windows service." : $"Service unavailable: {error.Message}", false); }
            finally
            {
                lock (gate) { if (ReferenceEquals(rpc, connection)) rpc = null; }
                connection?.Dispose();
                if (pipe is not null) await pipe.DisposeAsync();
            }
            if (!lifetime.IsCancellationRequested)
            {
                try { await Task.Delay(2000, lifetime.Token); } catch (OperationCanceledException) { break; }
            }
        }
    }
    public void Dispose() { lifetime.Cancel(); lock (gate) { rpc?.Dispose(); rpc = null; } }
    private sealed class Notifications(RpcService owner)
    {
        [JsonRpcMethod("job.progress")] public void OnProgress(Guid jobId, Progress progress) => owner.ProgressChanged?.Invoke(jobId, progress);
        [JsonRpcMethod("job.state")] public void OnState(Job job) => owner.JobChanged?.Invoke(job);
        [JsonRpcMethod("mounts.changed")] public void OnMounts() => owner.MountsChanged?.Invoke();
        [JsonRpcMethod("definitions.changed")] public void OnDefinitions() => owner.DefinitionsChanged?.Invoke();
    }
}
