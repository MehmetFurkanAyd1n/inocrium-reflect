using System.Text.Json;
using System.Text.Json.Serialization;

namespace Inocrium.App.Models;
public static class RpcErrorCodes
{
    public const int Cancelled = -32001;
    public const int NotFound = -32002;
    public const int Busy = -32003;
    public const int PasswordRequired = -32004;
    public const int BadPassword = -32005;
    public const int AccessDenied = -32006;
    public const int MethodNotFound = -32601;
}
public sealed record ServiceInfo(string Version, uint ProtocolVersion, string Machine);
public sealed record Partition(uint Number, ulong Offset, ulong Length, string TypeName, string? FileSystem, string? Label, string? Letter, ulong? UsedBytes, bool IsSystem, bool IsBoot, bool IsEsp);
public sealed record Disk(uint Number, string Model, string Serial, ulong Size, uint SectorSize, string Style, string DiskId, bool IsSystem, List<Partition> Partitions)
{
    public override string ToString() => $"Disk {Number} · {Model} · {ByteSize.Format(Size)}";
}
public sealed record DiskSelection(uint Number, string DiskId, List<uint>? Partitions);
public sealed record Retention(string Kind, uint Value);
public sealed record Schedule(string Kind, string Time, List<byte> Weekdays, byte? DayOfMonth);
public sealed class Definition
{
    public Guid Id { get; set; } = Guid.NewGuid();
    public string Name { get; set; } = "New backup";
    public bool Enabled { get; set; } = true;
    public List<DiskSelection> Disks { get; set; } = [];
    public string Destination { get; set; } = "";
    public string Prefix { get; set; } = "backup";
    public string Mode { get; set; } = "smart";
    public byte CompressionLevel { get; set; } = 3;
    public bool Encrypt { get; set; }
    public bool PasswordSet { get; set; }
    public bool Verify { get; set; } = true;
    public ulong? SplitSize { get; set; }
    public string Comment { get; set; } = "";
    public Retention? Retention { get; set; }
    public Schedule? Schedule { get; set; }
    public bool RunMissed { get; set; } = true;
    public DateTimeOffset? LastRun { get; set; }
    public DateTimeOffset? NextRun { get; set; }
}
public sealed record Progress(string Phase, string Detail, ulong BytesDone, ulong BytesTotal, ulong BytesWritten, ulong Rate)
{
    public double Percent => BytesTotal == 0 ? 0 : Math.Min(100, 100d * BytesDone / BytesTotal);
    public string Summary => $"{Phase} · {Detail} · {ByteSize.Format(BytesDone)} / {ByteSize.Format(BytesTotal)} · {ByteSize.Format(Rate)}/s · ETA {Eta}";
    public string Eta => Rate == 0 || BytesTotal < BytesDone ? "—" : TimeSpan.FromSeconds(Math.Min((BytesTotal - BytesDone) / (double)Rate, TimeSpan.MaxValue.TotalSeconds - 1)).ToString(@"d\.hh\:mm\:ss");
}
public sealed record Job(Guid Id, string Kind, string Title, string State, Progress Progress, DateTimeOffset? Started, DateTimeOffset? Finished, string? Error);
public sealed record ImagePartition(uint Number, ulong Length, string? FileSystem, string? Label, bool Captured, string Mode)
{
    public override string ToString() => $"Partition {Number} · {Label} · {FileSystem} · {ByteSize.Format(Length)}";
}
public sealed record ImageDisk(uint Index, ulong Size, string Style, List<ImagePartition> Partitions)
{
    public override string ToString() => $"Image disk {Index} · {Style} · {ByteSize.Format(Size)}";
}
public sealed record ImageSummary(string Path, DateTimeOffset Created, string Comment, ulong SizeOnDisk, bool Encrypted, Guid? DefinitionId, List<ImageDisk> Disks, bool Incomplete);
public sealed record Mount(Guid Id, string Image, uint Disk, uint? Partition, bool Writable, uint? DiskNumber, List<string> Letters);
public sealed record VirtualMachine(string Name, string State, string Image);
public sealed class Settings
{
    public string DefaultDestination { get; set; } = "";
    public uint LogRetentionDays { get; set; } = 30;
    public uint Threads { get; set; } = 4;
    [JsonExtensionData] public Dictionary<string, JsonElement> Extra { get; set; } = [];
}
public sealed record Credential(string Target, string User);
public sealed record HistoryEntry(Guid JobId, string Kind, string Title, string State, DateTimeOffset Started, DateTimeOffset? Finished, ulong Bytes, string? Error);
public static class ByteSize
{
    public static string Format(ulong bytes)
    {
        string[] units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
        double value = bytes;
        int unit = 0;
        while (value >= 1024 && unit < units.Length - 1) { value /= 1024; unit++; }
        return $"{value:0.##} {units[unit]}";
    }
}
