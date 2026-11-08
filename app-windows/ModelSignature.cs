using System.Collections;
using System.Collections.Concurrent;
using System.Globalization;
using System.Reflection;
using System.Text;

namespace Seedwell;

/// <summary>
/// Everything in a view model the core sent, as one string: two equal strings draw the same, so what is drawn can stay. Each
/// value by its own type, not the declared one: a model's blocks are subclasses of an abstract record, whose fields a
/// serializer given the declared type would leave out, and every model would then look the same.
/// </summary>
static class ModelSignature
{
    static readonly ConcurrentDictionary<Type, PropertyInfo[]> Properties = new();

    internal static string Of(object? model)
    {
        var text = new StringBuilder();
        Add(text, model);
        return text.ToString();
    }

    static void Add(StringBuilder text, object? value)
    {
        switch (value)
        {
            case null:
                text.Append('\u0001');
                break;
            case string s:
                // By its length too: no text can pass for the marks around it.
                text.Append(s.Length).Append(':').Append(s);
                break;
            case IFormattable number:
                text.Append(number.ToString(null, CultureInfo.InvariantCulture)).Append(';');
                break;
            case bool or char:
                text.Append(value).Append(';');
                break;
            case IEnumerable list:
                text.Append('[');
                foreach (var item in list) Add(text, item);
                text.Append(']');
                break;
            default:
                var type = value.GetType();
                text.Append(type.Name).Append('{');
                foreach (var property in Properties.GetOrAdd(type, static t => t.GetProperties(BindingFlags.Public | BindingFlags.Instance).Where(p => p.GetIndexParameters().Length == 0).ToArray()))
                {
                    Add(text, property.GetValue(value));
                }
                text.Append('}');
                break;
        }
    }
}
