using Inocrium.App.Models;
using Inocrium.App.ViewModels;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using System.Text.Json;
using System.Text.Json.Nodes;

namespace Inocrium.App;
public sealed partial class MainWindow
{
    private static TextBox Input(string label, string value = "") => new() { Header = label, Text = value, MinWidth = 380 };
    private static CheckBox Check(string label, bool value) => new() { Content = label, IsChecked = value };
    private static ComboBox Choice(string label, IEnumerable<string> choices, string selected)
    {
        var combo = new ComboBox { Header = label, MinWidth = 250 }; foreach (var choice in choices) combo.Items.Add(choice); combo.SelectedItem = selected; return combo;
    }
    private static NumberBox Number(string label, double value, double minimum, double maximum) => new() { Header = label, Value = value, Minimum = minimum, Maximum = maximum, SpinButtonPlacementMode = NumberBoxSpinButtonPlacementMode.Compact };
    private async Task CredentialAsync(string target = "")
    {
        var share = Input("Target (\\\\server\\share)", target); var user = Input("User (DOMAIN\\user)"); var password = new PasswordBox { Header = "Password" };
        if (await Dialog("Network credential", Stack(share, user, password), "Save credential").ShowAsync() != ContentDialogResult.Primary) return;
        try
        {
            if (!share.Text.StartsWith(@"\\") || string.IsNullOrWhiteSpace(user.Text)) throw new InvalidOperationException("Enter a UNC share and username.");
            await vm.Rpc.CallAsync("credentials.set", new { target = share.Text, user = user.Text, password = password.Password }); await RefreshAsync();
        }
        finally { password.Password = ""; }
    }
    private async Task EditDefinitionAsync(Definition? original, Disk? imageDisk = null)
    {
        var definition = original is null ? new Definition { Destination = vm.Settings.DefaultDestination } : JsonSerializer.Deserialize<Definition>(JsonSerializer.Serialize(original))!;
        if (imageDisk is not null) { definition.Name = $"Disk {imageDisk.Number} backup"; definition.Disks = [new(imageDisk.Number, imageDisk.DiskId, null)]; }
        var editor = new DefinitionViewModel(vm.Rpc, definition);
        var name = Input("Name", definition.Name); var destination = Input("Destination (local path or UNC)", definition.Destination); var prefix = Input("File prefix", definition.Prefix);
        var enabled = Check("Enabled", definition.Enabled); var runMissed = Check("Run missed scheduled backups", definition.RunMissed);
        var mode = Choice("Capture mode", ["smart", "forensic"], definition.Mode);
        var compression = new Slider { Header = "Compression level (0–19)", Minimum = 0, Maximum = 19, StepFrequency = 1, Value = definition.CompressionLevel };
        var encrypt = Check("Encrypt image", definition.Encrypt); var password = new PasswordBox { Header = definition.PasswordSet ? "New password (leave blank to retain stored password)" : "Encryption password" };
        var verify = Check("Verify after backup", definition.Verify); var split = Input("Split size in bytes (blank = single file)", definition.SplitSize?.ToString() ?? "");
        var comment = Input("Comment", definition.Comment); comment.AcceptsReturn = true;
        var retention = Choice("Retention", ["none", "keepLast", "keepDays"], definition.Retention?.Kind ?? "none"); var retentionValue = Number("Retention value", definition.Retention?.Value ?? 5, 1, uint.MaxValue);
        var schedule = Choice("Schedule (local machine time)", ["none", "daily", "weekly", "monthly"], definition.Schedule?.Kind ?? "none");
        var time = Input("Time (HH:MM)", definition.Schedule?.Time ?? "02:00"); var weekdays = Input("Weekdays (0=Sunday … 6=Saturday, comma separated)", string.Join(",", definition.Schedule?.Weekdays ?? [1])); var day = Number("Day of month", definition.Schedule?.DayOfMonth ?? 1, 1, 31);
        var diskControls = new List<(Disk Disk, CheckBox Whole, Dictionary<uint, CheckBox> Partitions)>();
        var content = Stack(name, enabled, Text("Select source disks / partitions", 18));
        foreach (var disk in vm.Disks)
        {
            var selection = definition.Disks.FirstOrDefault(d => d.Number == disk.Number && d.DiskId == disk.DiskId);
            var whole = Check($"Whole disk {disk.Number} · {disk.Model} · {ByteSize.Format(disk.Size)}", selection is not null && selection.Partitions is null);
            var parts = new Dictionary<uint, CheckBox>(); var diskPanel = Stack(whole);
            foreach (var partition in disk.Partitions) { var check = Check($"Partition {partition.Number} · {partition.Label} {partition.Letter} · {ByteSize.Format(partition.Length)}", selection?.Partitions?.Contains(partition.Number) == true); check.Margin = new Thickness(24, 0, 0, 0); parts[partition.Number] = check; diskPanel.Children.Add(check); }
            whole.Click += (_, _) => { foreach (var check in parts.Values) check.IsEnabled = whole.IsChecked != true; };
            foreach (var check in parts.Values) check.IsEnabled = whole.IsChecked != true;
            diskControls.Add((disk, whole, parts)); content.Children.Add(Card(diskPanel));
        }
        foreach (var control in new UIElement[] { destination, Row(Action("Choose destination…", async () => { var folder = await FolderAsync(); if (folder is not null) destination.Text = folder; })), prefix, mode, compression, encrypt, password, verify, split, comment, retention, retentionValue, schedule, time, weekdays, day, runMissed }) content.Children.Add(control);
        if (await Dialog(imageDisk is null ? "Backup definition" : "Image this disk", content, imageDisk is null ? "Save" : "Start backup").ShowAsync() != ContentDialogResult.Primary) { password.Password = ""; return; }
        try
        {
            definition.Disks = diskControls.Select(d => new DiskSelection(d.Disk.Number, d.Disk.DiskId, d.Whole.IsChecked == true ? null : d.Partitions.Where(p => p.Value.IsChecked == true).Select(p => p.Key).ToList())).Where(d => d.Partitions is null || d.Partitions.Count > 0).ToList();
            if (definition.Disks.Count == 0 || string.IsNullOrWhiteSpace(destination.Text)) throw new InvalidOperationException("Select at least one source disk/partition and enter a destination.");
            if (encrypt.IsChecked == true && string.IsNullOrEmpty(password.Password) && (!definition.PasswordSet || imageDisk is not null)) throw new InvalidOperationException("An encryption password is required.");
            definition.Name = name.Text; definition.Destination = destination.Text; definition.Prefix = prefix.Text; definition.Enabled = enabled.IsChecked == true;
            definition.Mode = mode.SelectedItem.ToString()!; definition.CompressionLevel = (byte)compression.Value; definition.Encrypt = encrypt.IsChecked == true; definition.Verify = verify.IsChecked == true;
            definition.SplitSize = string.IsNullOrWhiteSpace(split.Text) ? null : ulong.Parse(split.Text); if (definition.SplitSize == 0) throw new InvalidOperationException("Split size must be positive.");
            definition.Comment = comment.Text; definition.RunMissed = runMissed.IsChecked == true;
            definition.Retention = retention.SelectedItem.ToString() == "none" ? null : new Retention(retention.SelectedItem.ToString()!, checked((uint)retentionValue.Value));
            definition.Schedule = null;
            if (schedule.SelectedItem.ToString() != "none")
            {
                if (!TimeOnly.TryParseExact(time.Text, "HH:mm", out _)) throw new InvalidOperationException("Schedule time must be HH:MM.");
                var days = weekdays.Text.Split(',', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries).Select(byte.Parse).ToList();
                if (days.Any(d => d > 6) || (schedule.SelectedItem.ToString() == "weekly" && days.Count == 0)) throw new InvalidOperationException("Select valid weekdays 0–6.");
                definition.Schedule = new(schedule.SelectedItem.ToString()!, time.Text, days, checked((byte)day.Value));
            }
            if (destination.Text.StartsWith(@"\\") && !vm.Credentials.Any(c => destination.Text.StartsWith(c.Target, StringComparison.OrdinalIgnoreCase)))
            {
                if (await ConfirmAsync("Network destination", "The LocalSystem service may need a saved credential for this UNC share. Add one now?", "Add credential")) { var pieces = destination.Text.TrimStart('\\').Split('\\'); await CredentialAsync(pieces.Length >= 2 ? $@"\\{pieces[0]}\{pieces[1]}" : destination.Text); }
            }
            editor.Password = password.Password.Length == 0 ? null : password.Password;
            if (imageDisk is null) { await editor.SaveCommand.ExecuteAsync(null); if (editor.Error.Length > 0) throw new InvalidOperationException(editor.Error); }
            else { var parameters = JsonSerializer.SerializeToNode(definition, new JsonSerializerOptions { PropertyNamingPolicy = JsonNamingPolicy.CamelCase })!.AsObject(); parameters["password"] = editor.Password; await vm.Rpc.CallAsync<Guid>("backup.start", parameters); }
            await RefreshAsync();
        }
        finally { editor.Password = null; password.Password = ""; }
    }
    private async Task<string?> ImagePasswordAsync(ImageSummary image)
    {
        if (!image.Encrypted) return null;
        var password = new PasswordBox { Header = "Image password", MinWidth = 350 };
        if (await Dialog("Unlock encrypted image", password, "Continue").ShowAsync() != ContentDialogResult.Primary) { password.Password = ""; throw new OperationCanceledException("Image action cancelled."); }
        var result = password.Password; password.Password = ""; return result;
    }
    private async Task ImageActionAsync(ImageSummary image, string action)
    {
        if (action == "delete") { if (await ConfirmAsync("Delete image permanently?", $"This deletes the image files at {image.Path}.", "Delete image")) { await vm.Rpc.CallAsync("images.delete", new { path = image.Path }); await RefreshAsync(); } return; }
        if (image.Incomplete && action is "restore" or "viboot") throw new InvalidOperationException("An incomplete image cannot be restored or booted.");
        var password = await ImagePasswordAsync(image);
        try
        {
            if (action == "verify") { await vm.Rpc.CallAsync<Guid>("images.verify", new { path = image.Path, password }); await RefreshAsync(); return; }
            // Inspect provides authenticated, current metadata before choosing captured partitions.
            var details = await vm.Rpc.CallAsync<ImageSummary>("images.inspect", new { path = image.Path, password });
            if (action == "inspect") { await Dialog("Image details", Text(JsonSerializer.Serialize(details, new JsonSerializerOptions { WriteIndented = true })), "Close").ShowAsync(); return; }
            var diskChoice = new ComboBox { Header = "Image disk", ItemsSource = details.Disks, SelectedIndex = details.Disks.Count > 0 ? 0 : -1, MinWidth = 350 };
            var partitionChoice = new ComboBox { Header = "Captured partition (whole disk if unset)", MinWidth = 350 };
            void UpdatePartitions() { partitionChoice.Items.Clear(); partitionChoice.Items.Add("Whole disk"); if (diskChoice.SelectedItem is ImageDisk disk) foreach (var partition in disk.Partitions.Where(p => p.Captured)) partitionChoice.Items.Add(partition); partitionChoice.SelectedIndex = 0; }
            diskChoice.SelectionChanged += (_, _) => UpdatePartitions(); UpdatePartitions();
            var writable = Check("Writable mount (changes stored in an overlay)", false);
            var targets = new ComboBox { Header = "TARGET disk — existing data will be overwritten", ItemsSource = vm.Disks, MinWidth = 350 };
            var targetPartition = new ComboBox { Header = "Target partition (whole disk if unset)", MinWidth = 350 };
            targets.SelectionChanged += (_, _) => { targetPartition.Items.Clear(); targetPartition.Items.Add("Whole disk"); if (targets.SelectedItem is Disk target) foreach (var partition in target.Partitions) targetPartition.Items.Add(new ComboBoxItem { Content = $"Partition {partition.Number} · {partition.Label} · {ByteSize.Format(partition.Length)}", Tag = partition.Number }); targetPartition.SelectedIndex = 0; };
            var extend = Check("Extend restored partition to fill target", false); var offset = Input("Target offset in bytes (optional)");
            var vmName = Input("Virtual machine name (optional)"); var memory = Number("Memory (MiB)", 4096, 512, 1048576); var cpus = Number("Virtual CPUs", 2, 1, 256);
            var content = Stack(diskChoice);
            if (action is "mount" or "restore") content.Children.Add(partitionChoice);
            if (action == "mount") content.Children.Add(writable);
            if (action == "restore") foreach (var control in new UIElement[] { targets, targetPartition, offset, extend, Text("RESTORE DESTROYS DATA on the selected target. Check the disk identity and ensure you have a backup.", 16) }) content.Children.Add(control);
            if (action == "viboot") foreach (var control in new UIElement[] { vmName, memory, cpus }) content.Children.Add(control);
            if (await Dialog($"Image: {action}", content, "Continue").ShowAsync() != ContentDialogResult.Primary) return;
            var selectedDisk = diskChoice.SelectedItem as ImageDisk ?? throw new InvalidOperationException("Choose an image disk.");
            uint? sourcePartition = (partitionChoice.SelectedItem as ImagePartition)?.Number;
            switch (action)
            {
                case "mount": await vm.Rpc.CallAsync<Mount>("mounts.mount", new { path = image.Path, password, disk = selectedDisk.Index, partition = sourcePartition, writable = writable.IsChecked == true }); break;
                case "restore":
                    var target = targets.SelectedItem as Disk ?? throw new InvalidOperationException("Choose a target disk.");
                    uint? targetNumber = (targetPartition.SelectedItem as ComboBoxItem)?.Tag is uint number ? number : null;
                    if (sourcePartition is null && targetNumber is not null) throw new InvalidOperationException("Whole-disk restore requires a whole-disk target.");
                    var confirmation = Input($"Type RESTORE {target.Number} to confirm");
                    if (await Dialog("Destructive restore confirmation", Stack(Text($"Image {image.Path}\nTarget: {target}\nDisk ID: {target.DiskId}\nTarget partition: {targetNumber?.ToString() ?? "WHOLE DISK"}\nExisting data on this target will be destroyed."), confirmation), "Erase target and restore").ShowAsync() != ContentDialogResult.Primary) return;
                    if (confirmation.Text != $"RESTORE {target.Number}") throw new InvalidOperationException("Restore confirmation did not match.");
                    await vm.Rpc.CallAsync<Guid>("restore.start", new { path = image.Path, password, disk = selectedDisk.Index, partition = sourcePartition, targetDisk = target.Number, targetPartition = targetNumber, targetOffset = string.IsNullOrWhiteSpace(offset.Text) ? (ulong?)null : ulong.Parse(offset.Text), extend = extend.IsChecked == true }); break;
                case "export": var output = await SaveVhdxAsync(); if (output is null) return; await vm.Rpc.CallAsync<Guid>("vhdx.export", new { path = image.Path, password, disk = selectedDisk.Index, output }); break;
                case "viboot": await vm.Rpc.CallAsync<Guid>("viboot.start", new { path = image.Path, password, disk = selectedDisk.Index, vmName = string.IsNullOrWhiteSpace(vmName.Text) ? null : vmName.Text, memoryMb = checked((uint)memory.Value), cpus = checked((uint)cpus.Value) }); break;
            }
            await RefreshAsync();
        }
        finally { password = null; }
    }
}
