using System.Globalization;
using System.Text;
using System.Text.RegularExpressions;

namespace Depth;

/// UI text: English in source, translations from Resources\<lang>-*.txt ("English = translation"
/// per line, '#' comments), shared with the macOS host. Use with `using static Depth.I18n;`.
static class I18n
{
    static readonly Dictionary<string, Dictionary<string, string>> cache = new();

    /// The app language (state.lang, set from the status bar or Settings).
    public static string L(string en)
    {
        var lang = Store.Shared.State.Lang;
        return lang == "en" ? en : Table(lang).GetValueOrDefault(en, en);
    }

    static Dictionary<string, string> Table(string lang)
    {
        if (cache.TryGetValue(lang, out var t)) return t;
        var o = new Dictionary<string, string>();
        var dir = System.IO.Path.Combine(AppContext.BaseDirectory, "Resources");
        try
        {
            foreach (var f in Directory.GetFiles(dir, lang + "-*.txt").Order())
                foreach (var line in File.ReadLines(f, Encoding.UTF8))
                {
                    if (line.StartsWith('#')) continue;
                    int i = line.IndexOf('=');
                    if (i <= 0) continue;
                    string k = line[..i].Trim(), v = line[(i + 1)..].Trim();
                    if (v.Length > 0) o.TryAdd(k, v);
                }
        }
        catch (IOException) { }
        catch (UnauthorizedAccessException) { }
        return cache[lang] = o;
    }

    static readonly Regex spec = new(@"%(\+?)(?:\.(\d+))?([@fd%])");

    /// printf subset of the shared translation strings: %@, %d, %f, %.Nf, %+.Nf, %%.
    public static string F(string fmt, params object[] args)
    {
        int n = 0;
        return spec.Replace(fmt, m =>
        {
            if (m.Groups[3].Value == "%") return "%";
            var a = n < args.Length ? args[n++] : "";
            if (m.Groups[3].Value == "@") return a.ToString() ?? "";
            var v = Convert.ToDouble(a, CultureInfo.InvariantCulture);
            int d = m.Groups[3].Value == "d" ? 0 : m.Groups[2].Success ? int.Parse(m.Groups[2].Value, CultureInfo.InvariantCulture) : 6;
            return Fmt.F(v, d, m.Groups[1].Value == "+");
        });
    }
}
