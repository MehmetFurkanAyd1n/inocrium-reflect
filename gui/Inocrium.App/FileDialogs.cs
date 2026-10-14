using System.Runtime.InteropServices;

namespace Inocrium.App;
/// Win32 common item dialogs. The WinRT pickers fail in elevated processes, and this app
/// always runs elevated (requireAdministrator), so folder and file choices go through COM.
internal static class FileDialogs
{
    private const uint PickFolders = 0x20, ForceFileSystem = 0x40, NoChangeDir = 0x8, PathMustExist = 0x800, FileMustExist = 0x1000, OverwritePrompt = 0x2;
    private const uint FileSystemPath = 0x80058000;
    private const int Cancelled = unchecked((int)0x800704C7);
    private static readonly Guid OpenDialog = new("DC1C5A9C-E88A-4DDE-A5A1-60F82A20AEF7");
    private static readonly Guid SaveDialog = new("C0B4E2F3-BA21-4773-8DBA-335EC946EB8B");

    public static string? PickFolder(IntPtr owner, string title) => Show(OpenDialog, owner, title, PickFolders | ForceFileSystem | PathMustExist | NoChangeDir, [], null, null);
    public static string? PickFile(IntPtr owner, string title, params (string Name, string Pattern)[] filters) => Show(OpenDialog, owner, title, ForceFileSystem | PathMustExist | FileMustExist | NoChangeDir, filters, null, null);
    public static string? SaveFile(IntPtr owner, string title, string fileName, string extension, params (string Name, string Pattern)[] filters) => Show(SaveDialog, owner, title, ForceFileSystem | PathMustExist | OverwritePrompt | NoChangeDir, filters, fileName, extension);

    private static string? Show(Guid clsid, IntPtr owner, string title, uint options, (string Name, string Pattern)[] filters, string? fileName, string? extension)
    {
        var dialog = (IFileDialog)Activator.CreateInstance(Type.GetTypeFromCLSID(clsid, true)!)!;
        try
        {
            dialog.GetOptions(out uint current);
            dialog.SetOptions(current | options);
            dialog.SetTitle(title);
            if (filters.Length > 0) dialog.SetFileTypes((uint)filters.Length, filters.Select(f => new FilterSpec { Name = f.Name, Spec = f.Pattern }).ToArray());
            if (fileName is not null) dialog.SetFileName(fileName);
            if (extension is not null) dialog.SetDefaultExtension(extension);
            int result = dialog.Show(owner);
            if (result == Cancelled) return null;
            Marshal.ThrowExceptionForHR(result);
            dialog.GetResult(out IShellItem item);
            try
            {
                item.GetDisplayName(FileSystemPath, out IntPtr path);
                try { return Marshal.PtrToStringUni(path); }
                finally { Marshal.FreeCoTaskMem(path); }
            }
            finally { Marshal.ReleaseComObject(item); }
        }
        finally { Marshal.ReleaseComObject(dialog); }
    }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private struct FilterSpec
    {
        [MarshalAs(UnmanagedType.LPWStr)] public string Name;
        [MarshalAs(UnmanagedType.LPWStr)] public string Spec;
    }

    // IFileDialog including its IModalWindow base, in vtable order.
    [ComImport, Guid("42F85136-DB7E-439C-85F1-E4075D135FC8"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    private interface IFileDialog
    {
        [PreserveSig] int Show(IntPtr owner);
        void SetFileTypes(uint count, [MarshalAs(UnmanagedType.LPArray)] FilterSpec[] filters);
        void SetFileTypeIndex(uint index);
        void GetFileTypeIndex(out uint index);
        void Advise(IntPtr events, out uint cookie);
        void Unadvise(uint cookie);
        void SetOptions(uint options);
        void GetOptions(out uint options);
        void SetDefaultFolder(IShellItem folder);
        void SetFolder(IShellItem folder);
        void GetFolder(out IShellItem folder);
        void GetCurrentSelection(out IShellItem item);
        void SetFileName([MarshalAs(UnmanagedType.LPWStr)] string name);
        void GetFileName([MarshalAs(UnmanagedType.LPWStr)] out string name);
        void SetTitle([MarshalAs(UnmanagedType.LPWStr)] string title);
        void SetOkButtonLabel([MarshalAs(UnmanagedType.LPWStr)] string text);
        void SetFileNameLabel([MarshalAs(UnmanagedType.LPWStr)] string label);
        void GetResult(out IShellItem item);
        void AddPlace(IShellItem item, int place);
        void SetDefaultExtension([MarshalAs(UnmanagedType.LPWStr)] string extension);
        void Close(int result);
        void SetClientGuid(ref Guid guid);
        void ClearClientData();
        void SetFilter(IntPtr filter);
    }

    [ComImport, Guid("43826D1E-E718-42EE-BC55-A1E261C37BFE"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    private interface IShellItem
    {
        void BindToHandler(IntPtr context, ref Guid handler, ref Guid riid, out IntPtr result);
        void GetParent(out IShellItem parent);
        void GetDisplayName(uint kind, out IntPtr name);
        void GetAttributes(uint mask, out uint attributes);
        void Compare(IShellItem other, uint hint, out int order);
    }
}
