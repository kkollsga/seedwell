using System.Globalization;
using uniffi.trs_ffi;

namespace Seedwell;

/// <summary>
/// The user's own time and day formats for the core's texts (the Mac's system formats): Rust decides which form a text
/// uses, this only writes it, in the current culture and the local time zone.
/// </summary>
sealed class Clock : LocalClock
{
    static DateTimeOffset Local(ulong unixS) => DateTimeOffset.FromUnixTimeSeconds((long)unixS).ToLocalTime();

    static DateTimeFormatInfo Format => CultureInfo.CurrentCulture.DateTimeFormat;

    /// <summary>"22:29".</summary>
    public string TimeOfDay(ulong unixS) => Local(unixS).ToString("t", CultureInfo.CurrentCulture);

    /// <summary>"Mon 22:29".</summary>
    public string WeekdayTime(ulong unixS) => $"{Local(unixS).ToString("ddd", CultureInfo.CurrentCulture)} {TimeOfDay(unixS)}";

    /// <summary>"1 Nov" (the culture's month-day order with the short month name), or the short date with the year.</summary>
    public string Day(ulong unixS, bool withYear) => withYear
        ? Local(unixS).ToString("d", CultureInfo.CurrentCulture)
        : Local(unixS).ToString(Format.MonthDayPattern.Replace("MMMM", "MMM"), CultureInfo.CurrentCulture);

    /// <summary>A moment in full: the long date and the short time.</summary>
    public string DateTime(ulong unixS) => Local(unixS).ToString("f", CultureInfo.CurrentCulture);

    public int UtcOffsetS(ulong unixS) =>
        (int)TimeZoneInfo.Local.GetUtcOffset(DateTimeOffset.FromUnixTimeSeconds((long)unixS)).TotalSeconds;
}
