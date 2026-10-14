using System.Collections.ObjectModel;
using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using Inocrium.App.Models;
using Inocrium.App.Services;
using Microsoft.UI.Dispatching;

namespace Inocrium.App.ViewModels;
public partial class ShellViewModel : ObservableObject
{
    public RpcService Rpc { get; }
    private readonly DispatcherQueue dispatcher;
    private readonly SemaphoreSlim refreshGate = new(1);
    public ObservableCollection<Disk> Disks { get; } = [];
    public ObservableCollection<Definition> Definitions { get; } = [];
    public ObservableCollection<ImageSummary> Images { get; } = [];
    public ObservableCollection<Mount> Mounts { get; } = [];
    public ObservableCollection<Job> Jobs { get; } = [];
    public ObservableCollection<HistoryEntry> History { get; } = [];
    public ObservableCollection<Credential> Credentials { get; } = [];
    public ObservableCollection<VirtualMachine> VirtualMachines { get; } = [];
    public List<string> ImageFolders { get; } = [];
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
        try { await action(); } catch (Exception exception) { Error = exception.Message; }
    }
    [RelayCommand]
    private async Task RefreshAsync()
    {
        if (!Connected || !await refreshGate.WaitAsync(0)) return;
        try
        {
            // Independent capabilities may be implemented at different times by the service.
            await LoadAsync("disks.list", Disks);
            await LoadAsync("definitions.list", Definitions);
            await LoadAsync("images.list", Images, new { folders = ImageFolders.Count == 0 ? null : ImageFolders });
            await LoadAsync("mounts.list", Mounts);
            await LoadAsync("jobs.list", Jobs);
            await LoadAsync("history.list", History, new { limit = 100 });
            await LoadAsync("credentials.list", Credentials);
            await LoadAsync("viboot.list", VirtualMachines);
            await ExecuteAsync(async () => Settings = await Rpc.CallAsync<Settings>("settings.get"));
        }
        finally { refreshGate.Release(); }
    }
    private async Task LoadAsync<T>(string method, ObservableCollection<T> target, object? parameters = null) => await ExecuteAsync(async () => { var values = await Rpc.CallAsync<List<T>>(method, parameters); target.Clear(); foreach (var value in values) target.Add(value); });
    [RelayCommand] private async Task RunDefinitionAsync(Definition definition) => await ExecuteAsync(async () => { await Rpc.CallAsync<Guid>("definitions.run", new { id = definition.Id }); await RefreshAsync(); });
    [RelayCommand] private async Task DeleteDefinitionAsync(Definition definition) => await ExecuteAsync(async () => { await Rpc.CallAsync("definitions.delete", new { id = definition.Id }); await RefreshAsync(); });
    [RelayCommand] private async Task CancelJobAsync(Job job) => await ExecuteAsync(() => Rpc.CallAsync("jobs.cancel", new { id = job.Id }));
    [RelayCommand] private async Task UnmountAsync(Mount mount) => await ExecuteAsync(async () => { await Rpc.CallAsync("mounts.unmount", new { id = mount.Id }); await RefreshAsync(); });
    [RelayCommand] private async Task UnmountAllAsync() => await ExecuteAsync(async () => { await Rpc.CallAsync("mounts.unmount", new { id = "all" }); await RefreshAsync(); });
    [RelayCommand] private async Task DeleteCredentialAsync(Credential credential) => await ExecuteAsync(async () => { await Rpc.CallAsync("credentials.delete", new { target = credential.Target }); await RefreshAsync(); });
    [RelayCommand] private async Task StopVmAsync(VirtualMachine machine) => await ExecuteAsync(async () => { await Rpc.CallAsync("viboot.stop", new { name = machine.Name }); await RefreshAsync(); });
}
