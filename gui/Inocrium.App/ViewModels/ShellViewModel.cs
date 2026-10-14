using System.Collections.ObjectModel;
using System.Text.Json;
using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using Inocrium.App.Models;
using Inocrium.App.Services;
using Microsoft.UI.Dispatching;
using StreamJsonRpc;

namespace Inocrium.App.ViewModels;
public partial class ShellViewModel : ObservableObject
{
    public RpcService Rpc { get; }
    private readonly DispatcherQueue dispatcher;
    private readonly SemaphoreSlim refreshGate = new(1);
    private bool refreshPending;
    public ObservableCollection<Disk> Disks { get; } = [];
    public ObservableCollection<Definition> Definitions { get; } = [];
    public ObservableCollection<ImageSummary> Images { get; } = [];
    public ObservableCollection<Mount> Mounts { get; } = [];
    public ObservableCollection<Job> Jobs { get; } = [];
    public ObservableCollection<HistoryEntry> History { get; } = [];
    public ObservableCollection<Credential> Credentials { get; } = [];
    public ObservableCollection<VirtualMachine> VirtualMachines { get; } = [];
    [ObservableProperty] private string status = "Connecting to service…";
    [ObservableProperty] private string error = "";
    [ObservableProperty] private bool connected;
    [ObservableProperty] private Settings settings = new();
    public ShellViewModel(RpcService rpc)
    {
        Rpc = rpc; dispatcher = DispatcherQueue.GetForCurrentThread();
        rpc.ConnectionChanged += (message, online) => dispatcher.TryEnqueue(() => { Status = message; Connected = online; if (online) { Error = ""; RefreshCommand.Execute(null); } });
        rpc.ProgressChanged += (id, progress) => dispatcher.TryEnqueue(() => { var job = Jobs.FirstOrDefault(j => j.Id == id); if (job is not null) Jobs[Jobs.IndexOf(job)] = job with { Progress = progress }; });
        rpc.JobChanged += job => dispatcher.TryEnqueue(() => { var old = Jobs.FirstOrDefault(j => j.Id == job.Id); if (old is null) Jobs.Add(job); else Jobs[Jobs.IndexOf(old)] = job; if (job.State is "succeeded" or "failed" or "cancelled") RefreshCommand.Execute(null); });
        rpc.MountsChanged += () => dispatcher.TryEnqueue(() => RefreshCommand.Execute(null));
        rpc.DefinitionsChanged += () => dispatcher.TryEnqueue(() => RefreshCommand.Execute(null));
    }
    public async Task ExecuteAsync(Func<Task> action)
    {
        try { Error = ""; await action(); }
        catch (OperationCanceledException) { }
        catch (RemoteInvocationException exception) when (exception.ErrorCode == RpcErrorCodes.Busy) { Error = "The service is busy. Wait for the current operation and try again. If a mount backs a viBoot VM, stop that VM first."; }
        catch (RemoteInvocationException exception) when (exception.ErrorCode == RpcErrorCodes.AccessDenied) { Error = "Access denied. Run inocrium reflect as administrator."; }
        catch (Exception exception) { Error = exception.Message; }
    }
    [RelayCommand]
    private async Task RefreshAsync()
    {
        if (!Connected) return;
        refreshPending = true;
        if (!await refreshGate.WaitAsync(0)) return;
        try
        {
            do
            {
                refreshPending = false;
                await LoadAsync("disks.list", Disks);
                await LoadAsync("definitions.list", Definitions);
                await LoadAsync("images.list", Images);
                await LoadAsync("mounts.list", Mounts);
                await LoadAsync("jobs.list", Jobs);
                await LoadAsync("history.list", History, new { limit = 100 });
                await LoadAsync("credentials.list", Credentials);
                await LoadAsync("viboot.list", VirtualMachines);
                await ExecuteAsync(async () => { var current = await Rpc.CallAsync<Settings>("settings.get"); if (JsonSerializer.Serialize(current) != JsonSerializer.Serialize(Settings)) Settings = current; });
            } while (refreshPending && Connected);
        }
        finally { refreshGate.Release(); }
    }
    // Pages are rebuilt on every collection change; leaving unchanged lists alone keeps their
    // buttons alive, so a refresh between pointer press and release cannot swallow a click.
    private async Task LoadAsync<T>(string method, ObservableCollection<T> target, object? parameters = null) => await ExecuteAsync(async () =>
    {
        var values = await Rpc.CallAsync<List<T>>(method, parameters);
        if (JsonSerializer.Serialize(values) == JsonSerializer.Serialize(target.ToList())) return;
        target.Clear(); foreach (var value in values) target.Add(value);
    });
    [RelayCommand] private async Task RunDefinitionAsync(Definition definition) => await ExecuteAsync(async () =>
    {
        try { await Rpc.CallAsync<Guid>("definitions.run", new { id = definition.Id }); }
        catch (RemoteInvocationException error) when (RpcService.IsPasswordError(error))
        {
            // definitions.run accepts only an ID. Supply a transient password through
            // the flattened backup.start contract without changing the saved definition.
            var current = await Rpc.CallAsync<Definition>("definitions.get", new { id = definition.Id });
            var parameters = RpcService.BackupParameters(current);
            try { await Rpc.CallAsync<Guid>("backup.start", parameters); }
            finally { parameters.Remove("password"); }
        }
        await RefreshAsync();
    });
    [RelayCommand] private async Task DeleteDefinitionAsync(Definition definition) => await ExecuteAsync(async () => { await Rpc.CallAsync("definitions.delete", new { id = definition.Id }); await RefreshAsync(); });
    [RelayCommand] private async Task CancelJobAsync(Job job) => await ExecuteAsync(() => Rpc.CallAsync("jobs.cancel", new { id = job.Id }));
    [RelayCommand] private async Task UnmountAsync(Mount mount) => await ExecuteAsync(async () => { await Rpc.CallAsync("mounts.unmount", new { id = mount.Id }); await RefreshAsync(); });
    [RelayCommand] private async Task UnmountAllAsync() => await ExecuteAsync(async () => { await Rpc.CallAsync("mounts.unmount", new { id = "all" }); await RefreshAsync(); });
    [RelayCommand] private async Task DeleteCredentialAsync(Credential credential) => await ExecuteAsync(async () => { await Rpc.CallAsync("credentials.delete", new { target = credential.Target }); await RefreshAsync(); });
    [RelayCommand] private async Task StopVmAsync(VirtualMachine machine) => await ExecuteAsync(async () => { await Rpc.CallAsync("viboot.stop", new { name = machine.Name }); await RefreshAsync(); });
}
