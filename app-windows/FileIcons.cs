using System.Runtime.InteropServices;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Media.Imaging;

namespace Seedwell;

/// <summary>
/// The system's icon for a file type (the Mac's document icons): Explorer's own icon for the extension, or the folder
/// icon for an empty one, through SHGetFileInfo with SHGFI_USEFILEATTRIBUTES (no file is touched), from the system
/// image list that is crisp at the window's scaling: 32 px at 100-125 %, 48 px up to 150 %, the 256 px jumbo icons
/// scaled down above that. One per extension and size, kept for the app's life; null when Windows has none.
/// </summary>
static class FileIcons
{
    static readonly Dictionary<(string, int), ImageSource?> cache = [];

    /// <param name="scale">The window's rasterization scale (XamlRoot.RasterizationScale).</param>
    public static ImageSource? For(string ext, double scale)
    {
        ext = ext.ToLowerInvariant();
        int list = scale <= 1.25 ? SHIL_LARGE : scale <= 1.5 ? SHIL_EXTRALARGE : SHIL_JUMBO;
        if (cache.TryGetValue((ext, list), out var known))
        {
            return known;
        }
        ImageSource? image = null;
        try
        {
            image = Load(ext, list);
        }
        catch
        {
            // No icon: the caller draws its glyph.
        }
        cache[(ext, list)] = image;
        return image;
    }

    static ImageSource? Load(string ext, int list)
    {
        bool folder = ext.Length == 0;
        var info = new SHFILEINFO();
        // The type's index in the system image lists, then its icon from the list of the wanted size.
        var r = SHGetFileInfo(folder ? "folder" : $"file.{ext}", folder ? FILE_ATTRIBUTE_DIRECTORY : FILE_ATTRIBUTE_NORMAL, ref info, (uint)Marshal.SizeOf<SHFILEINFO>(), SHGFI_SYSICONINDEX | SHGFI_USEFILEATTRIBUTES);
        if (r == IntPtr.Zero)
        {
            return null;
        }
        var iid = typeof(IImageList).GUID;
        if (SHGetImageList(list, ref iid, out var images) != 0 || images is null)
        {
            return null;
        }
        IntPtr hIcon = IntPtr.Zero;
        if (images.GetIcon(info.iIcon, ILD_TRANSPARENT, ref hIcon) != 0 || hIcon == IntPtr.Zero)
        {
            return null;
        }
        try
        {
            using var icon = System.Drawing.Icon.FromHandle(hIcon);
            using var bitmap = icon.ToBitmap();
            var stream = new MemoryStream();
            bitmap.Save(stream, System.Drawing.Imaging.ImageFormat.Png);
            stream.Position = 0;
            var image = new BitmapImage();
            image.SetSource(stream.AsRandomAccessStream());
            return image;
        }
        finally
        {
            DestroyIcon(hIcon);
        }
    }

    const int SHIL_LARGE = 0x0, SHIL_EXTRALARGE = 0x2, SHIL_JUMBO = 0x4, ILD_TRANSPARENT = 0x1;
    const uint SHGFI_SYSICONINDEX = 0x4000, SHGFI_USEFILEATTRIBUTES = 0x10;
    const uint FILE_ATTRIBUTE_DIRECTORY = 0x10, FILE_ATTRIBUTE_NORMAL = 0x80;

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct SHFILEINFO
    {
        public IntPtr hIcon;
        public int iIcon;
        public uint dwAttributes;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 260)] public string szDisplayName;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 80)] public string szTypeName;
    }

    [DllImport("shell32.dll", CharSet = CharSet.Unicode)]
    static extern IntPtr SHGetFileInfo(string path, uint attributes, ref SHFILEINFO info, uint size, uint flags);

    [DllImport("user32.dll")]
    static extern bool DestroyIcon(IntPtr icon);

    [DllImport("shell32.dll")]
    static extern int SHGetImageList(int list, ref Guid iid, out IImageList? images);

    /// <summary>The system image list (commctrl IImageList): only GetIcon is used; the methods before it hold its place.</summary>
    [ComImport, Guid("46EB5926-582E-4017-9FDF-E8998DAA0950"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IImageList
    {
        [PreserveSig] int Add(IntPtr image, IntPtr mask, ref int index);
        [PreserveSig] int ReplaceIcon(int i, IntPtr icon, ref int index);
        [PreserveSig] int SetOverlayImage(int image, int overlay);
        [PreserveSig] int Replace(int i, IntPtr image, IntPtr mask);
        [PreserveSig] int AddMasked(IntPtr image, int mask, ref int index);
        [PreserveSig] int Draw(IntPtr parameters);
        [PreserveSig] int Remove(int i);
        [PreserveSig] int GetIcon(int i, int flags, ref IntPtr icon);
    }
}
