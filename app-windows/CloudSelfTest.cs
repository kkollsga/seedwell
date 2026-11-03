using System.Runtime.InteropServices;
using System.Text;

namespace Seedwell;

/// <summary>
/// QA's check of the Cloud Files plumbing on this PC, without the core and without anything of the user's (the QA
/// command <c>cloudfiles selftest</c>): a sync root in the run's own temp state, placeholders for a file and a folder,
/// a read served by the FETCH_DATA callback (the file is filled), pin and dehydrate, then everything taken down. It
/// proves the layouts and callbacks of <see cref="CloudFiles"/> before the host is wired to the core.
/// </summary>
static class CloudSelfTest
{
    // Kept alive while connected: the system calls through it.
    static CloudFiles.Callback? fetch;
    static readonly Dictionary<string, byte[]> contents = [];
    static int fetches;

    const uint RecallOnDataAccess = 0x00400000, Pinned = 0x00080000;

    internal static string Run(string stateDir)
    {
        var log = new StringBuilder();
        var root = Path.Combine(stateDir, "cloud-selftest", "root");
        Directory.CreateDirectory(root);
        long key = 0;
        bool registered = false;
        try
        {
            var identity = Encoding.UTF8.GetBytes("seedwell-selftest");
            var idHandle = GCHandle.Alloc(identity, GCHandleType.Pinned);
            try
            {
                var reg = new CloudFiles.SyncRegistration
                {
                    StructSize = (uint)Marshal.SizeOf<CloudFiles.SyncRegistration>(),
                    ProviderName = "Seedwell",
                    ProviderVersion = "1",
                    SyncRootIdentity = idHandle.AddrOfPinnedObject(),
                    SyncRootIdentityLength = (uint)identity.Length,
                    ProviderId = new Guid("5eed0e11-0000-4000-8000-000000000001"),
                };
                var policies = new CloudFiles.SyncPolicies
                {
                    StructSize = (uint)Marshal.SizeOf<CloudFiles.SyncPolicies>(),
                    HydrationPrimary = CloudFiles.HydrationFull,
                    PopulationPrimary = CloudFiles.PopulationAlwaysFull,
                    InSync = CloudFiles.InSyncTrackAll,
                };
                CloudFiles.Check(CloudFiles.CfRegisterSyncRoot(root, ref reg, ref policies, 0), "register");
                registered = true;
            }
            finally { idHandle.Free(); }
            log.Append("registered; ");

            fetch = OnFetch;
            var table = new[]
            {
                new CloudFiles.CallbackRegistration { Type = CloudFiles.CallbackType.FetchData, Callback = Marshal.GetFunctionPointerForDelegate(fetch) },
                new CloudFiles.CallbackRegistration { Type = CloudFiles.CallbackType.None, Callback = IntPtr.Zero },
            };
            CloudFiles.Check(CloudFiles.CfConnectSyncRoot(root, table, IntPtr.Zero, CloudFiles.ConnectRequireFullPath, out key), "connect");
            log.Append("connected; ");

            // A file and a folder with a file in it, as the feed would bring them.
            var hello = Encoding.UTF8.GetBytes("Hello from a Seedwell placeholder.\n");
            var inner = Encoding.UTF8.GetBytes(new string('x', 10_000));
            contents["f1"] = hello;
            contents["f2"] = inner;
            Create(root, "hello.txt", "f1", hello.Length, false);
            Create(root, "sub", "d1", 0, true);
            Create(Path.Combine(root, "sub"), "inner.txt", "f2", inner.Length, false);
            var helloPath = Path.Combine(root, "hello.txt");
            var innerPath = Path.Combine(root, "sub", "inner.txt");
            log.Append($"placeholders {(Attr(helloPath) & RecallOnDataAccess) != 0 && (Attr(innerPath) & RecallOnDataAccess) != 0}; ");
            log.Append($"sizes {new FileInfo(helloPath).Length}/{new FileInfo(innerPath).Length}; ");

            // A read fills the file through FETCH_DATA (off this thread: the callback runs on the system's).
            var read = ReadWithin(helloPath);
            log.Append($"read {(read is not null && read.AsSpan().SequenceEqual(hello) ? "matches" : "differs")}, fetches {fetches}; ");
            log.Append($"hydrated {(Attr(helloPath) & RecallOnDataAccess) == 0}; ");
            var innerRead = ReadWithin(innerPath);
            log.Append($"inner {(innerRead is not null && innerRead.AsSpan().SequenceEqual(inner) ? "matches" : "differs")}; ");

            using (var h = CloudFiles.Open(helloPath))
            {
                CloudFiles.Check(CloudFiles.CfSetPinState(h, CloudFiles.PinState.Pinned, 0, IntPtr.Zero), "pin");
            }
            log.Append($"pinned {(Attr(helloPath) & Pinned) != 0}; ");
            using (var h = CloudFiles.Open(helloPath))
            {
                CloudFiles.Check(CloudFiles.CfSetPinState(h, CloudFiles.PinState.Unspecified, 0, IntPtr.Zero), "unpin");
                CloudFiles.Check(CloudFiles.CfDehydratePlaceholder(h, 0, -1, 0, IntPtr.Zero), "dehydrate");
            }
            log.Append($"dehydrated {(Attr(helloPath) & RecallOnDataAccess) != 0}; ");
            var again = ReadWithin(helloPath);
            log.Append($"read again {(again is not null && again.AsSpan().SequenceEqual(hello) ? "matches" : "differs")}, fetches {fetches}");
        }
        catch (Exception e)
        {
            log.Append($"failed: {e.Message}");
        }
        finally
        {
            if (key != 0) CloudFiles.CfDisconnectSyncRoot(key);
            if (registered) CloudFiles.CfUnregisterSyncRoot(root);
            try { Directory.Delete(Path.GetDirectoryName(root)!, true); } catch (Exception e) { log.Append($"; cleanup: {e.Message}"); }
        }
        return log.ToString();
    }

    static uint Attr(string path) => (uint)File.GetAttributes(path);

    /// <summary>A file's bytes, read off this thread (the fill comes from the system's), at most 10 s.</summary>
    static byte[]? ReadWithin(string path)
    {
        var t = Task.Run(() => File.ReadAllBytes(path));
        return t.Wait(10_000) ? t.Result : null;
    }

    static void Create(string dir, string name, string id, long size, bool folder)
    {
        var bytes = Encoding.UTF8.GetBytes(id);
        var h = GCHandle.Alloc(bytes, GCHandleType.Pinned);
        try
        {
            var info = new[]
            {
                new CloudFiles.PlaceholderCreateInfo
                {
                    RelativeFileName = name,
                    FsMetadata = CloudFiles.Metadata(size, DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() * 1_000_000, folder),
                    FileIdentity = h.AddrOfPinnedObject(),
                    FileIdentityLength = (uint)bytes.Length,
                    // A folder is filled by the provider (always full): no on-demand population of its own.
                    Flags = CloudFiles.CreateMarkInSync | (folder ? 0x1u : 0u),
                },
            };
            CloudFiles.Check(CloudFiles.CfCreatePlaceholders(dir, info, 1, 0, out _), $"create {name}");
            CloudFiles.Check(info[0].Result, $"create {name} (result)");
        }
        finally { h.Free(); }
    }

    /// <summary>FETCH_DATA: the whole file the identity names, from offset 0 (full hydration).</summary>
    static void OnFetch(IntPtr infoPtr, IntPtr parameters)
    {
        var info = Marshal.PtrToStructure<CloudFiles.CallbackInfo>(infoPtr);
        Interlocked.Increment(ref fetches);
        var id = Encoding.UTF8.GetString(ToBytes(info.FileIdentity, (int)info.FileIdentityLength));
        var (want, wantLength) = CloudFiles.FetchRange(parameters);
        CloudFiles.Transfer(info, contents.TryGetValue(id, out var data) ? data : null, data is null ? want : 0, length: wantLength);
    }

    static byte[] ToBytes(IntPtr p, int length)
    {
        var b = new byte[length];
        if (length > 0) Marshal.Copy(p, b, 0, length);
        return b;
    }
}
