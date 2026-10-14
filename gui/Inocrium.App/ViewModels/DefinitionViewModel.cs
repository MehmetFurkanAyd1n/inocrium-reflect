using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using Inocrium.App.Models;
using Inocrium.App.Services;

namespace Inocrium.App.ViewModels;
public partial class DefinitionViewModel(RpcService rpc, Definition definition) : ObservableObject
{
    public Definition Definition { get; } = definition;
    [ObservableProperty] private string error = "";
    [ObservableProperty] private string? password;
    [RelayCommand]
    private async Task SaveAsync()
    {
        try { Error = ""; Definition.Id = await rpc.CallAsync<Guid>("definitions.save", new { definition = Definition, password = Password }); }
        catch (OperationCanceledException) { throw; }
        catch (Exception exception) { Error = exception.Message; }
        finally { Password = null; }
    }
}
