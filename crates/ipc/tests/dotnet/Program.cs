using System.Text.Json;
using Inocrium.App.Models;
using Inocrium.App.Services;
using StreamJsonRpc;

using var client = new RpcService(args[0]);
var connected = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
var reconnected = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
var progress = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
var state = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
var mounts = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
var definitions = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
int connects = 0;
client.ConnectionChanged += (message, online) => { Console.WriteLine(message); if (online) { if (Interlocked.Increment(ref connects) == 1) connected.TrySetResult(); else reconnected.TrySetResult(); } };
client.ProgressChanged += (jobId, value) => { if (value.BytesDone == 123 && value.Phase == "copy" && jobId != Guid.Empty) progress.TrySetResult(); };
client.JobChanged += job => { if (job.State == "running" && job.Progress.BytesDone == 123) state.TrySetResult(); };
client.MountsChanged += () => mounts.TrySetResult();
client.DefinitionsChanged += () => definitions.TrySetResult();
client.Start();
await connected.Task.WaitAsync(TimeSpan.FromSeconds(10));
var echo = await client.CallAsync<string>("echo", new { value = "Türkçe ✓" });
if (echo != "Türkçe ✓") throw new Exception("UTF-8 round trip failed");
try { await client.CallAsync<JsonElement>("fail"); throw new Exception("Expected RPC error"); }
catch (RemoteInvocationException error) when (error.ErrorCode == -32005) { }
await client.CallAsync("notify");
await Task.WhenAll(progress.Task, state.Task, mounts.Task, definitions.Task).WaitAsync(TimeSpan.FromSeconds(5));
try { await client.CallAsync("restart"); }
catch (Exception error) when (error is ConnectionLostException or ObjectDisposedException or OperationCanceledException) { }
await reconnected.Task.WaitAsync(TimeSpan.FromSeconds(15));
if (await client.CallAsync<string>("echo", new { value = "after restart" }) != "after restart") throw new Exception("Reconnect failed");
Console.WriteLine("PASS: StreamJsonRpc framing, typed DTOs, errors, all notifications and service restart.");
