using Inocrium.App.Models;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using System.Diagnostics;
using System.Text.Json;

namespace Inocrium.App;
public sealed partial class MainWindow
{
    private void DisksPage()
    {
        foreach (var disk in vm.Disks)
        {
            var bar = new Grid { Height = 64, HorizontalAlignment = HorizontalAlignment.Stretch };
            var intervals = disk.Partitions.OrderBy(p => p.Offset).ToList(); ulong cursor = 0;
            void AddSegment(ulong length, Partition? partition)
            {
                if (length == 0) return;
                int column = bar.ColumnDefinitions.Count;
                bar.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(length / (double)Math.Max(1UL, disk.Size), GridUnitType.Star) });
                var color = partition?.FileSystem?.ToUpperInvariant() switch { "NTFS" => Microsoft.UI.Colors.SteelBlue, "FAT32" => Microsoft.UI.Colors.SeaGreen, "EXFAT" => Microsoft.UI.Colors.DarkGoldenrod, "REFS" => Microsoft.UI.Colors.MediumPurple, _ => Microsoft.UI.Colors.DimGray };
                string label = partition is null ? $"Unallocated · {ByteSize.Format(length)}" : $"{partition.Label} {partition.Letter} · {partition.FileSystem ?? partition.TypeName} · {ByteSize.Format(length)}";
                var segment = new Border { Background = new SolidColorBrush(color), BorderBrush = new SolidColorBrush(Microsoft.UI.Colors.White), BorderThickness = new Thickness(1), Padding = new Thickness(5), Child = new TextBlock { Text = label, TextTrimming = TextTrimming.CharacterEllipsis, Foreground = new SolidColorBrush(Microsoft.UI.Colors.White), VerticalAlignment = VerticalAlignment.Center } };
                ToolTipService.SetToolTip(segment, label); Grid.SetColumn(segment, column); bar.Children.Add(segment);
            }
            foreach (var partition in intervals) { if (partition.Offset > cursor) AddSegment(partition.Offset - cursor, null); AddSegment(partition.Length, partition); cursor = Math.Max(cursor, partition.Offset + partition.Length); }
            if (cursor < disk.Size) AddSegment(disk.Size - cursor, null);
            page.Children.Add(Card(Stack(Text(disk.ToString(), 20), Text($"{disk.Style.ToUpperInvariant()} · ID {disk.DiskId} · sector {disk.SectorSize} B · serial {disk.Serial}"), bar, Action("Image this disk…", () => EditDefinitionAsync(null, disk)))));
        }
        if (vm.Disks.Count == 0) page.Children.Add(Text("No disks received from the service."));
    }
    private void DefinitionsPage()
    {
        page.Children.Add(Action("New definition…", () => EditDefinitionAsync(null)));
        foreach (var definition in vm.Definitions) page.Children.Add(Card(Stack(Text(definition.Name, 20), Text($"{definition.Destination} · {definition.Mode} · compression {definition.CompressionLevel} · {(definition.Encrypt ? "encrypted" : "unencrypted")}"), Row(Action("Edit…", () => EditDefinitionAsync(definition)), Command("Run now", vm.RunDefinitionCommand, definition), Action("Delete", async () => { if (await ConfirmAsync("Delete definition?", definition.Name, "Delete")) await vm.DeleteDefinitionCommand.ExecuteAsync(definition); })))));
    }
    private void SchedulePage()
    {
        foreach (var definition in vm.Definitions)
        {
            var enabled = new CheckBox { Content = "Enabled", IsChecked = definition.Enabled };
            var missed = new CheckBox { Content = "Run missed backups", IsChecked = definition.RunMissed };
            enabled.Click += async (_, _) => { try { await SaveScheduleAsync(definition, enabled.IsChecked == true, missed.IsChecked == true); } catch (Exception error) { vm.Error = error.Message; } };
            missed.Click += async (_, _) => { try { await SaveScheduleAsync(definition, enabled.IsChecked == true, missed.IsChecked == true); } catch (Exception error) { vm.Error = error.Message; } };
            page.Children.Add(Card(Stack(Text(definition.Name, 20), Text($"Schedule: {definition.Schedule?.Kind ?? "manual"} {definition.Schedule?.Time}\nLast run: {definition.LastRun?.ToLocalTime().ToString("g") ?? "—"} · Next run: {definition.NextRun?.ToLocalTime().ToString("g") ?? "—"}"), Row(enabled, missed), Action("Edit schedule…", () => EditDefinitionAsync(definition)))));
        }
    }
    private async Task SaveScheduleAsync(Definition definition, bool enabled, bool missed)
    {
        var updated = JsonSerializer.Deserialize<Definition>(JsonSerializer.Serialize(definition))!;
        updated.Enabled = enabled; updated.RunMissed = missed;
        await vm.ExecuteAsync(async () => { await vm.Rpc.CallAsync<Guid>("definitions.save", new { definition = updated, password = (string?)null }); await RefreshAsync(); });
    }
    private void ImagesPage()
    {
        var typed = new TextBox { Header = "Image folder (local path or UNC)", PlaceholderText = @"D:\Backups", MinWidth = 380 };
        page.Children.Add(Row(Action("Add image folder…", async () => { var path = await FolderAsync("Choose a folder that contains images"); if (path is not null) await AddImageFolderAsync(path); }),
            Action("Open image file…", async () => { var file = await ImageFileAsync(); if (file is not null) await AddImageFolderAsync(Path.GetDirectoryName(file)!); })));
        var add = Action("Add folder", () => AddImageFolderAsync(typed.Text)); add.VerticalAlignment = VerticalAlignment.Bottom;
        page.Children.Add(Row(typed, add));
        var searched = new StackPanel { Spacing = 6 };
        searched.Children.Add(Text("Folders searched for images", 18));
        foreach (var folder in ImageFolderSetting())
        {
            var remove = Action("Remove", async () => await SaveImageFoldersAsync(ImageFolderSetting().Where(f => !string.Equals(f, folder, StringComparison.OrdinalIgnoreCase)).ToList()));
            searched.Children.Add(Row(Text(folder), remove));
        }
        if (!string.IsNullOrWhiteSpace(vm.Settings.DefaultDestination)) searched.Children.Add(Text($"{vm.Settings.DefaultDestination} (default destination)"));
        foreach (var definition in vm.Definitions.Where(d => !string.IsNullOrWhiteSpace(d.Destination))) searched.Children.Add(Text($"{definition.Destination} (definition \"{definition.Name}\")"));
        if (searched.Children.Count == 1) searched.Children.Add(Text("None yet. Add the folder your backups are in."));
        page.Children.Add(Card(searched));
        if (vm.Images.Count == 0) page.Children.Add(Text("No images found in these folders."));
        foreach (var image in vm.Images)
        {
            var actions = new StackPanel { Spacing = 6 };
            foreach (var (title, operation) in new[] { ("Inspect", "inspect"), ("Mount…", "mount"), ("Verify", "verify"), ("Restore…", "restore"), ("Export VHDX…", "export"), ("viBoot…", "viboot"), ("Delete", "delete") }) actions.Children.Add(Action(title, () => ImageActionAsync(image, operation)));
            page.Children.Add(Card(Stack(Text(image.Path, 18), Text($"{image.Created.ToLocalTime():g} · {ByteSize.Format(image.SizeOnDisk)} · {(image.Encrypted ? "Encrypted" : "Not encrypted")} · {(image.Incomplete ? "INCOMPLETE" : "Complete")}\n{image.Comment}"), Text(string.Join("\n", image.Disks.Select(d => $"Disk {d.Index}: {ByteSize.Format(d.Size)} · {d.Style} · {d.Partitions.Count(p => p.Captured)} captured partitions"))), actions)));
        }
        page.Children.Add(Text("viBoot virtual machines", 20));
        foreach (var machine in vm.VirtualMachines) page.Children.Add(Card(Row(Text($"{machine.Name} · {machine.State}"), Command("Stop", vm.StopVmCommand, machine))));
    }
    private List<string> ImageFolderSetting() =>
        vm.Settings.Extra.TryGetValue("imageFolders", out var value) && value.ValueKind == JsonValueKind.Array
            ? value.EnumerateArray().Where(e => e.ValueKind == JsonValueKind.String).Select(e => e.GetString()!).ToList()
            : [];
    private async Task AddImageFolderAsync(string folder)
    {
        folder = folder.Trim().Trim('"');
        if (!Path.IsPathFullyQualified(folder)) throw new InvalidOperationException(@"Enter a full folder path, such as D:\Backups or \\server\share\backups.");
        var folders = ImageFolderSetting();
        if (folders.Contains(folder, StringComparer.OrdinalIgnoreCase)) { await RefreshAsync(); return; }
        folders.Add(folder);
        await SaveImageFoldersAsync(folders);
    }
    /// Image folders live in the service's settings, so the service and every client search them.
    private async Task SaveImageFoldersAsync(List<string> folders)
    {
        var current = vm.Settings;
        var settings = new Settings { DefaultDestination = current.DefaultDestination, Threads = current.Threads, LogRetentionDays = current.LogRetentionDays, Extra = new(current.Extra) };
        settings.Extra["imageFolders"] = JsonSerializer.SerializeToElement(folders);
        await vm.Rpc.CallAsync("settings.set", new { settings });
        await RefreshAsync();
    }
    private void MountsPage()
    {
        page.Children.Add(Command("Unmount all", vm.UnmountAllCommand));
        foreach (var mount in vm.Mounts) page.Children.Add(Card(Stack(Text(mount.Image, 18), Text($"Image disk {mount.Disk}, partition {mount.Partition?.ToString() ?? "whole disk"} · {(mount.Writable ? "writable" : "read-only")} · {string.Join(", ", mount.Letters)}"), Row(Command("Unmount", vm.UnmountCommand, mount), Action("Open in Explorer", () => { var letter = mount.Letters.FirstOrDefault(); if (letter is null) throw new InvalidOperationException("This mount has no drive letter."); string drive = letter.TrimEnd(':', '\\'); if (drive.Length != 1 || !char.IsAsciiLetter(drive[0])) throw new InvalidOperationException("Invalid drive letter from service."); Process.Start(new ProcessStartInfo("explorer.exe") { UseShellExecute = true, Arguments = drive + ":\\" }); return Task.CompletedTask; })))));
    }
    private void ActivityPage()
    {
        jobControls.Clear();
        foreach (var job in vm.Jobs)
        {
            var progress = new ProgressBar { Minimum = 0, Maximum = 100 }; var summary = Text("");
            ShowProgress(progress, summary, job); jobControls[job.Id] = (progress, summary);
            var card = Stack(Text($"{job.Title} · {job.State}", 18), progress, summary, Text(job.Error ?? ""));
            if (job.State is "queued" or "running") card.Children.Add(Command("Cancel", vm.CancelJobCommand, job));
            page.Children.Add(Card(card));
        }
        page.Children.Add(Text("History", 22));
        foreach (var entry in vm.History) page.Children.Add(Card(Stack(Text($"{entry.Title} · {entry.State}", 18), Text($"{entry.Started.ToLocalTime():g} → {entry.Finished?.ToLocalTime().ToString("g")} · {ByteSize.Format(entry.Bytes)}\n{entry.Error}"), Action("View log", async () => { var log = await vm.Rpc.CallAsync<string>("logs.read", new { jobId = entry.JobId }); await Dialog("Job log", new TextBox { Text = log, IsReadOnly = true, AcceptsReturn = true, TextWrapping = TextWrapping.Wrap, MinWidth = 500 }, "Close").ShowAsync(); }))));
    }
    private static void ShowProgress(ProgressBar bar, TextBlock summary, Job job)
    {
        bar.Value = job.Progress.Percent;
        bar.IsIndeterminate = job.State == "queued" || (job.State == "running" && job.Progress.BytesTotal == 0);
        summary.Text = job.Progress.Summary;
    }
    private void SettingsPage()
    {
        var destination = new TextBox { Header = "Default destination (local path or UNC)", Text = vm.Settings.DefaultDestination };
        var threads = new NumberBox { Header = "Threads", Value = vm.Settings.Threads, Minimum = 1, Maximum = 256, SpinButtonPlacementMode = NumberBoxSpinButtonPlacementMode.Compact };
        var retention = new NumberBox { Header = "Log retention (days)", Value = vm.Settings.LogRetentionDays, Minimum = 1, Maximum = 3650 };
        page.Children.Add(Card(Stack(destination, Row(Action("Choose folder…", async () => { var path = await FolderAsync(); if (path is not null) destination.Text = path; }), Action("Save settings", async () => { if (!double.IsFinite(threads.Value) || !double.IsFinite(retention.Value)) throw new InvalidOperationException("Enter valid thread and retention values."); var settings = new Settings { DefaultDestination = destination.Text, Threads = checked((uint)threads.Value), LogRetentionDays = checked((uint)retention.Value), Extra = vm.Settings.Extra }; await vm.Rpc.CallAsync("settings.set", new { settings }); await RefreshAsync(); })), threads, retention)));
        page.Children.Add(Text("Network credentials", 20)); page.Children.Add(Action("Add credential…", () => CredentialAsync()));
        foreach (var credential in vm.Credentials) page.Children.Add(Card(Row(Text($"{credential.Target} · {credential.User}"), Command("Remove", vm.DeleteCredentialCommand, credential))));
        page.Children.Add(Card(Stack(Text("inocrium reflect", 22), Text("Open-source Windows disk imaging · protocol 1\nLicensed under GNU GPL version 3 only (GPL-3.0-only). You may use, study, modify and redistribute this software under that license. Distributed without warranty.\nSource and full license: https://github.com/MehmetFurkanAyd1n/inocrium-reflect"))));
    }
}
