using Inocrium.App.Models;
using Inocrium.App.ViewModels;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Windows.Storage.Pickers;

namespace Inocrium.App;
public sealed partial class MainWindow : Window
{
    private readonly ShellViewModel vm;
    private readonly NavigationView navigation = new() { PaneDisplayMode = NavigationViewPaneDisplayMode.Left, IsSettingsVisible = false };
    private readonly InfoBar connectionBar = new() { IsOpen = true, IsClosable = false, Severity = InfoBarSeverity.Warning };
    private readonly InfoBar errorBar = new() { IsClosable = true, Severity = InfoBarSeverity.Error, Title = "Action failed" };
    private readonly StackPanel page = new() { Spacing = 16, Margin = new Thickness(28) };
    private string currentPage = "Disks";
    public MainWindow(ShellViewModel viewModel)
    {
        vm = viewModel; Title = "inocrium reflect"; SystemBackdrop = new MicaBackdrop();
        AppWindow.Resize(new Windows.Graphics.SizeInt32(1200, 850));
        var root = new Grid(); root.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto }); root.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto }); root.RowDefinitions.Add(new RowDefinition());
        root.Children.Add(connectionBar); Grid.SetRow(errorBar, 1); root.Children.Add(errorBar); Grid.SetRow(navigation, 2); root.Children.Add(navigation);
        navigation.Content = new ScrollViewer { Content = page, HorizontalScrollBarVisibility = ScrollBarVisibility.Disabled };
        foreach (var name in new[] { "Disks", "Backup definitions", "Schedule", "Existing images", "Mounts", "Activity / History", "Settings" }) navigation.MenuItems.Add(new NavigationViewItem { Content = name, Tag = name });
        navigation.SelectionChanged += (_, args) => { currentPage = ((NavigationViewItem)args.SelectedItem).Tag.ToString()!; RenderPage(); };
        vm.PropertyChanged += (_, args) =>
        {
            if (args.PropertyName is nameof(vm.Status) or nameof(vm.Connected)) { connectionBar.Message = vm.Status; connectionBar.IsOpen = !vm.Connected; }
            if (args.PropertyName == nameof(vm.Error)) { errorBar.Message = vm.Error; errorBar.IsOpen = vm.Error.Length > 0; }
            if (args.PropertyName == nameof(vm.Settings) && currentPage == "Settings") RenderPage();
        };
        vm.Disks.CollectionChanged += (_, _) => Changed("Disks");
        vm.Definitions.CollectionChanged += (_, _) => { Changed("Backup definitions"); Changed("Schedule"); };
        vm.Images.CollectionChanged += (_, _) => Changed("Existing images");
        vm.Mounts.CollectionChanged += (_, _) => Changed("Mounts");
        vm.Jobs.CollectionChanged += (_, _) => Changed("Activity / History");
        vm.History.CollectionChanged += (_, _) => Changed("Activity / History");
        vm.Credentials.CollectionChanged += (_, _) => Changed("Settings");
        vm.VirtualMachines.CollectionChanged += (_, _) => Changed("Existing images");
        Content = root; navigation.SelectedItem = navigation.MenuItems[0];
        connectionBar.Message = vm.Status; vm.Rpc.Start();
    }
    private void Changed(string name) { if (currentPage == name) RenderPage(); }
    private void RenderPage()
    {
        page.Children.Clear();
        page.Children.Add(Text(currentPage, 28));
        page.Children.Add(new Button { Content = "Refresh", Command = vm.RefreshCommand });
        switch (currentPage)
        {
            case "Disks": DisksPage(); break;
            case "Backup definitions": DefinitionsPage(); break;
            case "Schedule": SchedulePage(); break;
            case "Existing images": ImagesPage(); break;
            case "Mounts": MountsPage(); break;
            case "Activity / History": ActivityPage(); break;
            case "Settings": SettingsPage(); break;
        }
    }
    private static TextBlock Text(string text, double size = 14) => new() { Text = text, FontSize = size, TextWrapping = TextWrapping.Wrap };
    private static StackPanel Stack(params UIElement[] children) { var panel = new StackPanel { Spacing = 10 }; foreach (var child in children) panel.Children.Add(child); return panel; }
    private static StackPanel Row(params UIElement[] children) { var row = Stack(children); row.Orientation = Orientation.Horizontal; return row; }
    private static Border Card(UIElement child) => new() { Child = child, Padding = new Thickness(20), CornerRadius = new CornerRadius(8), BorderThickness = new Thickness(1), BorderBrush = new SolidColorBrush(Microsoft.UI.Colors.Gray), Background = (Brush)Application.Current.Resources["CardBackgroundFillColorDefaultBrush"] };
    private Button Action(string title, Func<Task> action)
    {
        var button = new Button { Content = title };
        button.Click += async (_, _) =>
        {
            try { button.IsEnabled = false; await vm.ExecuteAsync(action); }
            catch (Exception exception) { vm.Error = exception.Message; }
            finally { button.IsEnabled = true; }
        };
        return button;
    }
    private static Button Command(string title, System.Windows.Input.ICommand command, object? parameter = null) => new() { Content = title, Command = command, CommandParameter = parameter };
    private ContentDialog Dialog(string title, UIElement content, string primary = "OK") => new() { XamlRoot = ((FrameworkElement)Content).XamlRoot, Title = title, Content = new ScrollViewer { Content = content, MaxHeight = 600 }, PrimaryButtonText = primary, CloseButtonText = "Cancel", DefaultButton = ContentDialogButton.Close };
    private async Task<bool> ConfirmAsync(string title, string text, string action) => await Dialog(title, Text(text), action).ShowAsync() == ContentDialogResult.Primary;
    private async Task<string?> FolderAsync()
    {
        var picker = new FolderPicker(); picker.FileTypeFilter.Add("*");
        WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(this));
        return (await picker.PickSingleFolderAsync())?.Path;
    }
    private async Task<string?> SaveVhdxAsync()
    {
        var picker = new FileSavePicker { SuggestedFileName = "disk" }; picker.FileTypeChoices.Add("Virtual hard disk", new List<string> { ".vhdx" });
        WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(this));
        return (await picker.PickSaveFileAsync())?.Path;
    }
    private async Task RefreshAsync() => await vm.RefreshCommand.ExecuteAsync(null);
}
