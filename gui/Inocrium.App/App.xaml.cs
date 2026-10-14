using Inocrium.App.Services;
using Inocrium.App.ViewModels;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.UI.Xaml;

namespace Inocrium.App;
public partial class App : Application
{
    private Window? window;
    public static ServiceProvider Services { get; } = new ServiceCollection()
        .AddSingleton<RpcService>().AddSingleton<ShellViewModel>().BuildServiceProvider();
    public App() => InitializeComponent();
    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        window = new MainWindow(Services.GetRequiredService<ShellViewModel>());
        window.Closed += (_, _) => { Services.GetRequiredService<RpcService>().Dispose(); Services.Dispose(); };
        window.Activate();
    }
}
