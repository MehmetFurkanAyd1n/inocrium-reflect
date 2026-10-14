using System.IO.Pipes;
using System.Text.Json;
using System.Text.Json.Nodes;
using Inocrium.App.Models;
using StreamJsonRpc;
using StreamJsonRpc.Protocol;

namespace Inocrium.App.Services;
public sealed class RpcService(string pipeName = "inocrium-reflect") : IDisposable
{
    private readonly CancellationTokenSource lifetime = new();
    private readonly object gate = new();
    private JsonRpc? rpc;
    private Task? reconnect;
    private bool disposed;
    private static readonly JsonSerializerOptions WireJson = new() { PropertyNamingPolicy = JsonNamingPolicy.CamelCase };
    public Func<bool, Task<string?>>? PasswordRequested { get; set; }
    public event Action<string, bool>? ConnectionChanged;
    public event Action<Guid, Progress>? ProgressChanged;
    public event Action<Job>? JobChanged;
    public event Action? MountsChanged;
    public event Action? DefinitionsChanged;
    public void Start() { lock (gate) { ObjectDisposedException.ThrowIf(disposed, this); reconnect ??= ReconnectAsync(); } }
    public bool IsConnected { get { lock (gate) return rpc is { IsDisposed: false }; } }
    public Task<T> CallAsync<T>(string method, object? parameters = null) => CallAsync<T>(method, parameters, null);
    /// `accepted` receives a password the user entered after a rejection, once the call succeeds
    /// with it, so the caller can reuse it for follow-up calls on the same image.
    public async Task<T> CallAsync<T>(string method, object? parameters, Action<string>? accepted)
    {
        JsonObject? retryParameters = null;
        try
        {
            while (true)
            {
                try
                {
                    var result = await SendAsync<T>(method, retryParameters ?? parameters);
                    if (accepted is not null && retryParameters?["password"]?.GetValue<string>() is { } used) accepted(used);
                    return result;
                }
                catch (RemoteInvocationException error) when (IsPasswordError(error) && PasswordRequested is not null && SupportsPassword(method))
                {
                    // Only rejected authentication requests are retried. Never replay a request
                    // after a timeout/disconnect: a mount or backup may already have started.
                    var password = await PasswordRequested(error.ErrorCode == RpcErrorCodes.BadPassword);
                    if (password is null) throw new OperationCanceledException("Password entry cancelled.");
                    retryParameters ??= JsonSerializer.SerializeToNode(parameters ?? new { }, WireJson)!.AsObject();
                    retryParameters["password"] = password;
                    password = null;
                }
            }
        }
        finally { retryParameters?.Remove("password"); }
    }
    public static bool IsPasswordError(RemoteInvocationException error) => error.ErrorCode is RpcErrorCodes.PasswordRequired or RpcErrorCodes.BadPassword;
    private static bool SupportsPassword(string method) => method is "definitions.save" or "backup.start" or "images.inspect" or "images.verify" or "mounts.mount" or "restore.start" or "vhdx.export" or "viboot.start" or "credentials.set";
    public static JsonObject BackupParameters(Definition definition, string? password = null)
    {
        var parameters = JsonSerializer.SerializeToNode(definition, WireJson)!.AsObject();
        parameters["password"] = password;
        return parameters;
    }
    private async Task<T> SendAsync<T>(string method, object? parameters)
    {
        JsonRpc connection;
        lock (gate) connection = rpc ?? throw new IOException("The service is not connected.");
        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(lifetime.Token);
        timeout.CancelAfter(TimeSpan.FromSeconds(method is "mounts.mount" or "mounts.unmount" or "viboot.start" ? 180 : 30));
        try { return await connection.InvokeWithParameterObjectAsync<T>(method, parameters ?? new { }, timeout.Token); }
        catch (OperationCanceledException) when (!lifetime.IsCancellationRequested)
        {
            throw new TimeoutException($"The {method} request timed out. Check Activity and Mounts before retrying; the service may have completed the operation.");
        }
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
                connection = new ContractJsonRpc(handler, new Notifications(this));
                connection.StartListening();
                // Publish the connection only after its protocol has been checked.
                using var handshakeTimeout = CancellationTokenSource.CreateLinkedTokenSource(lifetime.Token);
                handshakeTimeout.CancelAfter(TimeSpan.FromSeconds(30));
                var info = await connection.InvokeWithParameterObjectAsync<ServiceInfo>("service.info", new { }, handshakeTimeout.Token);
                if (info.ProtocolVersion != 1) throw new IOException($"Unsupported service protocol {info.ProtocolVersion}; this GUI requires 1.");
                lock (gate) { lifetime.Token.ThrowIfCancellationRequested(); rpc = connection; }
                ConnectionChanged?.Invoke($"Connected to {info.Machine} · service {info.Version}", true);
                await connection.Completion.WaitAsync(lifetime.Token);
                ConnectionChanged?.Invoke("Service disconnected. Reconnecting…", false);
            }
            catch (OperationCanceledException) when (lifetime.IsCancellationRequested) { break; }
            catch (UnauthorizedAccessException) { ConnectionChanged?.Invoke("Access denied. Run inocrium reflect as administrator.", false); }
            catch (Exception error) { ConnectionChanged?.Invoke(error is TimeoutException ? "Service is not running. Start the InocriumReflect Windows service." : $"Service unavailable: {error.Message}", false); }
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
    public void Dispose() { lock (gate) { if (disposed) return; disposed = true; lifetime.Cancel(); rpc?.Dispose(); rpc = null; PasswordRequested = null; } }
    private sealed class ContractJsonRpc(IJsonRpcMessageHandler handler, object target) : JsonRpc(handler, target)
    {
        // Rust's application codes overlap StreamJsonRpc's private extension codes
        // (notably -32003). Preserve the wire code instead of treating BUSY as a
        // serialization failure or interpreting error.data as a .NET exception.
        protected override RemoteRpcException CreateExceptionFromRpcError(JsonRpcRequest request, JsonRpcError response)
        {
            var error = response.Error ?? throw new InvalidDataException("The service returned an error response without error details.");
            return new RemoteInvocationException(error.Message, (int)error.Code, error.Data);
        }
        protected override Type? GetErrorDetailsDataType(JsonRpcError error) => typeof(JsonElement);
    }
    private sealed class Notifications(RpcService owner)
    {
        [JsonRpcMethod("job.progress")] public void OnProgress(Guid jobId, Progress progress) => owner.ProgressChanged?.Invoke(jobId, progress);
        [JsonRpcMethod("job.state")] public void OnState(Job job) => owner.JobChanged?.Invoke(job);
        [JsonRpcMethod("mounts.changed")] public void OnMounts() => owner.MountsChanged?.Invoke();
        [JsonRpcMethod("definitions.changed")] public void OnDefinitions() => owner.DefinitionsChanged?.Invoke();
    }
}
