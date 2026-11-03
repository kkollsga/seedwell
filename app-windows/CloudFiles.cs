using System.Runtime.InteropServices;

namespace Seedwell;

/// <summary>
/// The Cloud Files API (cfapi.h, cldapi.dll) as Seedwell's on-demand folders use it (item 6, SPEC 7G.11): sync roots,
/// placeholders, hydration and the callbacks. Plumbing only: what goes in a placeholder, when it is filled and what a
/// callback answers are the core's (<c>sync_ondemand.rs</c>, <c>sync_host.rs</c>); the host's part is in CloudHost.
/// Layouts follow the x64 Windows SDK headers.
/// </summary>
static class CloudFiles
{
    // ------------------------------------------------------------------------------------------- constants

    internal const ushort HydrationFull = 2;               // CF_HYDRATION_POLICY_FULL
    internal const ushort PopulationAlwaysFull = 3;        // CF_POPULATION_POLICY_ALWAYS_FULL
    internal const uint InSyncTrackAll = 0x00ffffff;       // CF_INSYNC_POLICY_TRACK_ALL
    internal const uint RegisterUpdate = 0x1;              // CF_REGISTER_FLAG_UPDATE
    internal const uint ConnectRequireFullPath = 0x4;      // CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH
    internal const uint CreateMarkInSync = 0x2;            // CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC
    internal const uint UpdateMarkInSync = 0x2;            // CF_UPDATE_FLAG_MARK_IN_SYNC
    internal const uint UpdateDehydrate = 0x4;             // CF_UPDATE_FLAG_DEHYDRATE
    internal const uint UpdateClearInSync = 0x40;          // CF_UPDATE_FLAG_CLEAR_IN_SYNC
    internal const uint ConvertMarkInSync = 0x1;           // CF_CONVERT_FLAG_MARK_IN_SYNC
    internal const uint ConvertAlwaysFull = 0x8;           // CF_CONVERT_FLAG_ALWAYS_FULL

    internal enum CallbackType : uint
    {
        FetchData = 0,
        CancelFetchData = 2,
        NotifyFileCloseCompletion = 6,
        NotifyDehydrate = 7,
        NotifyDelete = 9,
        NotifyRename = 11,
        None = 0xffffffff,
    }

    internal enum OperationType : uint
    {
        TransferData = 0,
        AckDehydrate = 5,
        AckDelete = 6,
        AckRename = 7,
    }

    internal enum PinState : uint { Unspecified = 0, Pinned = 1, Unpinned = 2, Excluded = 3, Inherit = 4 }

    // NTSTATUS values the callbacks answer with (ntstatus.h).
    internal const int StatusSuccess = 0;
    internal const int StatusCloudFileUnsuccessful = unchecked((int)0xC000CF12);
    internal const int StatusCloudFileNetworkUnavailable = unchecked((int)0xC000CF11);
    internal const int StatusCloudFileProviderNotRunning = unchecked((int)0xC000CF01);
    internal const int StatusCloudFileNotInSync = unchecked((int)0xC000CF08);
    internal const int StatusCloudFileInUse = unchecked((int)0xC000CF14);
    internal const int StatusCloudFileAccessDenied = unchecked((int)0xC000CF18);
    internal const int StatusObjectNameNotFound = unchecked((int)0xC0000034);

    // ------------------------------------------------------------------------------------------- structures

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    internal struct SyncRegistration
    {
        public uint StructSize;
        public string ProviderName;
        public string ProviderVersion;
        public IntPtr SyncRootIdentity;
        public uint SyncRootIdentityLength;
        public IntPtr FileIdentity;
        public uint FileIdentityLength;
        public Guid ProviderId;
    }

    [StructLayout(LayoutKind.Sequential)]
    internal struct SyncPolicies
    {
        public uint StructSize;
        public ushort HydrationPrimary;
        public ushort HydrationModifier;
        public ushort PopulationPrimary;
        public ushort PopulationModifier;
        public uint InSync;
        public uint HardLink;
        public uint PlaceholderManagement;
    }

    [StructLayout(LayoutKind.Sequential)]
    internal struct CallbackRegistration
    {
        public CallbackType Type;
        public IntPtr Callback;
    }

    /// <summary>CF_CALLBACK_INFO (read in place: the system owns it for the callback's length).</summary>
    [StructLayout(LayoutKind.Sequential)]
    internal struct CallbackInfo
    {
        public uint StructSize;
        public long ConnectionKey;
        public IntPtr CallbackContext;
        public IntPtr VolumeGuidName;
        public IntPtr VolumeDosName;
        public uint VolumeSerialNumber;
        public long SyncRootFileId;
        public IntPtr SyncRootIdentity;
        public uint SyncRootIdentityLength;
        public long FileId;
        public long FileSize;
        public IntPtr FileIdentity;
        public uint FileIdentityLength;
        public IntPtr NormalizedPath;
        public long TransferKey;
        public byte PriorityHint;
        public IntPtr CorrelationVector;
        public IntPtr ProcessInfo;
        public long RequestKey;
    }

    [StructLayout(LayoutKind.Sequential)]
    internal struct OperationInfo
    {
        public uint StructSize;
        public OperationType Type;
        public long ConnectionKey;
        public long TransferKey;
        public IntPtr CorrelationVector;
        public IntPtr SyncStatus;
        public long RequestKey;
    }

    /// <summary>CF_OPERATION_PARAMETERS with the TransferData member (the largest the host sends).</summary>
    [StructLayout(LayoutKind.Explicit, Size = 40)]
    internal struct OperationParameters
    {
        [FieldOffset(0)] public uint ParamSize;
        [FieldOffset(8)] public uint Flags;
        [FieldOffset(12)] public int CompletionStatus;
        [FieldOffset(16)] public IntPtr Buffer;
        [FieldOffset(24)] public long Offset;
        [FieldOffset(32)] public long Length;
    }

    [StructLayout(LayoutKind.Sequential)]
    internal struct FsMetadata
    {
        public long CreationTime;
        public long LastAccessTime;
        public long LastWriteTime;
        public long ChangeTime;
        public uint FileAttributes;
        public long FileSize;
    }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    internal struct PlaceholderCreateInfo
    {
        public string RelativeFileName;
        public FsMetadata FsMetadata;
        public IntPtr FileIdentity;
        public uint FileIdentityLength;
        public uint Flags;
        public int Result;
        public long CreateUsn;
    }

    /// <summary>The callback's shape (CF_CALLBACK): the info and the parameters, both owned by the system.</summary>
    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    internal delegate void Callback(IntPtr info, IntPtr parameters);

    // ------------------------------------------------------------------------------------------- functions

    [DllImport("cldapi.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
    internal static extern int CfRegisterSyncRoot(string syncRootPath, ref SyncRegistration registration, ref SyncPolicies policies, uint flags);

    [DllImport("cldapi.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
    internal static extern int CfUnregisterSyncRoot(string syncRootPath);

    [DllImport("cldapi.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
    internal static extern int CfConnectSyncRoot(string syncRootPath, [In] CallbackRegistration[] callbackTable, IntPtr callbackContext, uint flags, out long connectionKey);

    [DllImport("cldapi.dll", ExactSpelling = true)]
    internal static extern int CfDisconnectSyncRoot(long connectionKey);

    [DllImport("cldapi.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
    internal static extern int CfCreatePlaceholders(string baseDirectoryPath, [In, Out] PlaceholderCreateInfo[] placeholders, uint count, uint flags, out uint processed);

    [DllImport("cldapi.dll", ExactSpelling = true)]
    internal static extern int CfExecute(ref OperationInfo info, ref OperationParameters parameters);

    [DllImport("cldapi.dll", ExactSpelling = true)]
    internal static extern int CfReportProviderProgress(long connectionKey, long transferKey, long total, long completed);

    [DllImport("cldapi.dll", ExactSpelling = true)]
    internal static extern int CfUpdatePlaceholder(Microsoft.Win32.SafeHandles.SafeFileHandle file, IntPtr fsMetadata, IntPtr fileIdentity, uint fileIdentityLength, IntPtr dehydrateRanges, uint dehydrateRangeCount, uint flags, IntPtr updateUsn, IntPtr overlapped);

    [DllImport("cldapi.dll", ExactSpelling = true)]
    internal static extern int CfSetInSyncState(Microsoft.Win32.SafeHandles.SafeFileHandle file, uint inSyncState, uint flags, IntPtr inSyncUsn);

    [DllImport("cldapi.dll", ExactSpelling = true)]
    internal static extern int CfSetPinState(Microsoft.Win32.SafeHandles.SafeFileHandle file, PinState pinState, uint flags, IntPtr overlapped);

    [DllImport("cldapi.dll", ExactSpelling = true)]
    internal static extern int CfHydratePlaceholder(Microsoft.Win32.SafeHandles.SafeFileHandle file, long startingOffset, long length, uint flags, IntPtr overlapped);

    [DllImport("cldapi.dll", ExactSpelling = true)]
    internal static extern int CfDehydratePlaceholder(Microsoft.Win32.SafeHandles.SafeFileHandle file, long startingOffset, long length, uint flags, IntPtr overlapped);

    [DllImport("cldapi.dll", ExactSpelling = true)]
    internal static extern int CfConvertToPlaceholder(Microsoft.Win32.SafeHandles.SafeFileHandle file, IntPtr fileIdentity, uint fileIdentityLength, uint flags, IntPtr convertUsn, IntPtr overlapped);

    [DllImport("cldapi.dll", ExactSpelling = true)]
    internal static extern int CfRevertPlaceholder(Microsoft.Win32.SafeHandles.SafeFileHandle file, uint flags, IntPtr overlapped);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern Microsoft.Win32.SafeHandles.SafeFileHandle CreateFileW(string name, uint access, uint share, IntPtr security, uint disposition, uint flags, IntPtr template);

    const uint FileReadAttributes = 0x80, FileWriteAttributes = 0x100, ShareAll = 0x7, OpenExisting = 3, BackupSemantics = 0x02000000;

    /// <summary>A handle on a file or folder for the Cf* calls that take one (attributes only: opening never hydrates).</summary>
    internal static Microsoft.Win32.SafeHandles.SafeFileHandle Open(string path)
    {
        var h = CreateFileW(path, FileReadAttributes | FileWriteAttributes, ShareAll, IntPtr.Zero, OpenExisting, BackupSemantics, IntPtr.Zero);
        if (h.IsInvalid) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error(), path);
        return h;
    }

    /// <summary>A file opened to read its data (what CfHydratePlaceholder needs), shared with everyone.</summary>
    internal static Microsoft.Win32.SafeHandles.SafeFileHandle OpenForData(string path)
    {
        var h = CreateFileW(path, GenericRead, ShareAll, IntPtr.Zero, OpenExisting, BackupSemantics, IntPtr.Zero);
        if (h.IsInvalid) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error(), path);
        return h;
    }

    const uint GenericRead = 0x80000000;

    /// <summary>An HRESULT the Cf* calls return: thrown with the system's words when it failed.</summary>
    internal static void Check(int hr, string what)
    {
        if (hr < 0) throw new System.ComponentModel.Win32Exception(hr, $"{what}: 0x{hr:X8}");
    }

    /// <summary>A placeholder's metadata: its size, times (Unix nanoseconds) and whether it is a folder.</summary>
    internal static FsMetadata Metadata(long size, long mtimeNs, bool dir)
    {
        // FILETIME: 100 ns since 1601.
        long ft = mtimeNs / 100 + 116444736000000000L;
        return new FsMetadata
        {
            CreationTime = ft,
            LastAccessTime = ft,
            LastWriteTime = ft,
            ChangeTime = ft,
            FileAttributes = dir ? 0x10u /* DIRECTORY */ : 0x80u /* NORMAL */,
            FileSize = dir ? 0 : size,
        };
    }

    /// <summary>
    /// Answers a FETCH_DATA with <paramref name="data"/>'s bytes at <paramref name="offset"/> (whole 4 KB pages, the
    /// last one may be short), or, when <paramref name="data"/> is null, with a failure status for the range the request
    /// asked for (<paramref name="offset"/>, <paramref name="length"/>: <see cref="FetchRange"/>; the system refuses a
    /// failure of any other range, and the read that asked then waits on).
    /// </summary>
    internal static int Transfer(in CallbackInfo info, byte[]? data, long offset, int status = StatusSuccess, long length = 0)
    {
        var op = new OperationInfo
        {
            StructSize = (uint)Marshal.SizeOf<OperationInfo>(),
            Type = OperationType.TransferData,
            ConnectionKey = info.ConnectionKey,
            TransferKey = info.TransferKey,
            RequestKey = info.RequestKey,
        };
        var handle = data is null ? default : GCHandle.Alloc(data, GCHandleType.Pinned);
        try
        {
            var p = new OperationParameters
            {
                ParamSize = 40,
                CompletionStatus = data is null ? (status == StatusSuccess ? StatusCloudFileUnsuccessful : status) : StatusSuccess,
                Buffer = data is null ? IntPtr.Zero : handle.AddrOfPinnedObject(),
                Offset = offset,
                Length = data?.Length ?? length,
            };
            return CfExecute(ref op, ref p);
        }
        finally
        {
            if (handle.IsAllocated) handle.Free();
        }
    }

    /// <summary>The range a FETCH_DATA must fill (CF_CALLBACK_PARAMETERS.FetchData: RequiredFileOffset, RequiredLength).</summary>
    internal static (long Offset, long Length) FetchRange(IntPtr parameters) => (Marshal.ReadInt64(parameters, 16), Marshal.ReadInt64(parameters, 24));

    /// <summary>A notification's flags (CF_CALLBACK_PARAMETERS: Delete, Rename, CloseCompletion all start with them).</summary>
    internal static uint NotifyFlags(IntPtr parameters) => (uint)Marshal.ReadInt32(parameters, 8);

    /// <summary>A rename's target, as a full path (CF_CALLBACK_PARAMETERS.Rename.TargetPath is relative to the volume).</summary>
    internal static string RenameTarget(in CallbackInfo info, IntPtr parameters) =>
        Marshal.PtrToStringUni(info.VolumeDosName) + Marshal.PtrToStringUni(Marshal.ReadIntPtr(parameters, 16));

    internal const uint RenameIsDirectory = 0x1, RenameTargetInScope = 0x4;   // CF_CALLBACK_RENAME_FLAGS
    internal const uint CloseDeleted = 0x1;                                   // CF_CALLBACK_CLOSE_COMPLETION_FLAG_DELETED
    internal const uint DeleteIsUndelete = 0x2;                               // CF_CALLBACK_DELETE_FLAG_IS_UNDELETE

    /// <summary>Whether a file or folder is a placeholder (the host's), not one the user made.</summary>
    internal static bool IsPlaceholder(string path)
    {
        using var h = Open(path);
        var info = new byte[8];   // FILE_ATTRIBUTE_TAG_INFO: FileAttributes, ReparseTag
        if (!GetFileInformationByHandleEx(h, 9, info, (uint)info.Length)) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error(), path);
        return (CfGetPlaceholderStateFromAttributeTag(BitConverter.ToUInt32(info, 0), BitConverter.ToUInt32(info, 4)) & 0x1) != 0;
    }

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetFileInformationByHandleEx(Microsoft.Win32.SafeHandles.SafeFileHandle file, int infoClass, byte[] info, uint size);

    [DllImport("cldapi.dll", ExactSpelling = true)]
    static extern uint CfGetPlaceholderStateFromAttributeTag(uint fileAttributes, uint reparseTag);

    /// <summary>
    /// A file or folder of the user's made a placeholder: <paramref name="id"/> its identity, in sync (it holds its
    /// content); a folder is filled by the provider (always full), as the folders it creates.
    /// </summary>
    internal static void Convert(string path, string id, bool dir)
    {
        var bytes = System.Text.Encoding.UTF8.GetBytes(id);
        var pin = GCHandle.Alloc(bytes, GCHandleType.Pinned);
        try
        {
            using var h = Open(path);
            Check(CfConvertToPlaceholder(h, pin.AddrOfPinnedObject(), (uint)bytes.Length, ConvertMarkInSync | (dir ? ConvertAlwaysFull : 0), IntPtr.Zero, IntPtr.Zero), "convert");
        }
        finally { pin.Free(); }
    }

    /// <summary>Acknowledges a notification (delete, rename, dehydrate) with <paramref name="status"/>.</summary>
    internal static int Ack(in CallbackInfo info, OperationType type, int status)
    {
        var op = new OperationInfo
        {
            StructSize = (uint)Marshal.SizeOf<OperationInfo>(),
            Type = type,
            ConnectionKey = info.ConnectionKey,
            TransferKey = info.TransferKey,
            RequestKey = info.RequestKey,
        };
        var p = new OperationParameters { ParamSize = type == OperationType.AckDehydrate ? 32u : 16u, CompletionStatus = status };
        return CfExecute(ref op, ref p);
    }
}
