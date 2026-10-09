using System.Globalization;
using Microsoft.UI;
using Microsoft.UI.Input;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Documents;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Media.Imaging;
using Windows.UI;
using Colors = Microsoft.UI.Colors;
using CoreVirtualKeyStates = Windows.UI.Core.CoreVirtualKeyStates;
using Ellipse = Microsoft.UI.Xaml.Shapes.Ellipse;
using VirtualKey = Windows.System.VirtualKey;

namespace Depth;

/// Palette shared with the Rust side (ui/src/theme.rs); accent and up/down follow Settings.
/// Colors resolve against the window's effective theme at the time they are read: a theme switch
/// raises Store.Changed with every section dirty, and views rebuild with the new values.
static class T1
{
    /// effective theme of the window content (MainWindow keeps it current)
    public static bool IsDark = true;

    static Color C(uint h) => Color.FromArgb(255, (byte)(h >> 16), (byte)(h >> 8), (byte)h);
    static Color D(uint dark, uint light) => C(IsDark ? dark : light);

    public static Color Bg => D(0x0a0d11, 0xffffff);
    public static Color Panel => D(0x12161c, 0xf5f5f7);
    public static Color Panel2 => D(0x1a1f27, 0xececf0);
    public static Color Hl => D(0x20262f, 0xe3e3e8);
    public static Color Line => D(0x242a33, 0xd8d8de);
    public static Color Fg => D(0xe6e9ed, 0x1d1d1f);
    public static Color Mu => D(0x8a929d, 0x6e6e73);
    public static Color Dim => D(0x59616c, 0x9a9aa0);
    public static readonly Color Warn = C(0xe8a33d);
    public static readonly Color Green = C(0x30d158);
    public static readonly Color Red = C(0xff453a);
    public static readonly Color Orange = C(0xff9f0a);

    static Prefs P => Store.Shared.State.Prefs;
    public static Color Accent => P.Accent.Count == 3 ? Color.FromArgb(255, (byte)P.Accent[0], (byte)P.Accent[1], (byte)P.Accent[2]) : C(0x4c9eeb);
    /// rising prices; red when Settings asks for the East Asian convention
    public static Color Up => P.RedUp ? Red : Green;
    public static Color Down => P.RedUp ? Green : Red;

    public static SolidColorBrush B(Color c, double opacity = 1) => new(c) { Opacity = opacity };
    public static readonly SolidColorBrush Clear = new(Colors.Transparent);
    /// "#rrggbb" (venue colors from the Rust side)
    public static Color Hex(string s) => uint.TryParse(s.TrimStart('#'), NumberStyles.HexNumber, CultureInfo.InvariantCulture, out var h) ? C(h) : C(0x888888);

    /// Display time zone offset (Settings > General), the system's by default.
    public static TimeSpan Offset(DateTimeOffset at) => P.TzMin is int m ? TimeSpan.FromMinutes(m) : TimeZoneInfo.Local.GetUtcOffset(at);
    public static DateTimeOffset Local(DateTimeOffset at) => at.ToOffset(Offset(at));
    /// "UTC", "UTC+8", "UTC-3:30"
    public static string TzLabel(TimeSpan off)
    {
        int m = (int)Math.Round(off.TotalMinutes), a = Math.Abs(m);
        if (m == 0) return "UTC";
        return "UTC" + (m < 0 ? "-" : "+") + (a % 60 == 0 ? $"{a / 60}" : $"{a / 60}:{a % 60:00}");
    }

    static readonly Dictionary<string, BitmapImage?> venueImgs = new();
    static readonly Dictionary<string, BitmapImage> coinImgs = new();

    /// Exchange logo (Icons\<venue>.png next to the exe), else a dot.
    public static FrameworkElement VenueIcon(string ex, double size = 14)
    {
        var key = ex.ToLowerInvariant();
        if (!venueImgs.TryGetValue(key, out var img))
        {
            var p = System.IO.Path.Combine(AppContext.BaseDirectory, "Icons", key + ".png");
            img = File.Exists(p) ? new BitmapImage(new Uri(p)) : null;
            venueImgs[key] = img;
        }
        if (img == null) return new Ellipse { Width = size * 0.6, Height = size * 0.6, Margin = new Thickness(size * 0.2), Fill = B(Mu), VerticalAlignment = VerticalAlignment.Center };
        return new Ellipse { Width = size, Height = size, Fill = new ImageBrush { ImageSource = img, Stretch = Stretch.UniformToFill }, VerticalAlignment = VerticalAlignment.Center };
    }

    /// Coin logo (Binance CDN, the same source the Rust side caches) over a lettered disc.
    public static FrameworkElement CoinIcon(string b, double size = 18)
    {
        if (!coinImgs.TryGetValue(b, out var img))
            coinImgs[b] = img = new BitmapImage(new Uri($"https://bin.bnbstatic.com/static/assets/logos/{b}.png")) { DecodePixelWidth = 64 };
        var g = new Grid { Width = size, Height = size, VerticalAlignment = VerticalAlignment.Center };
        g.Children.Add(new Ellipse { Fill = B(Hl) });
        g.Children.Add(new TextBlock
        {
            Text = b.Length > 0 ? b[..1] : "", FontSize = size * 0.5, FontWeight = FontWeights.SemiBold, Foreground = B(Mu),
            HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center,
        });
        // a failed download leaves the brush empty and the disc shows
        g.Children.Add(new Ellipse { Fill = new ImageBrush { ImageSource = img, Stretch = Stretch.UniformToFill } });
        return g;
    }
}

/// Number formatting matching the Rust side (fmt_px / fmt_qty) and the macOS host.
static class Fmt
{
    static readonly CultureInfo Inv = CultureInfo.InvariantCulture;

    public static string Px(double? v)
    {
        if (v is not double x || !double.IsFinite(x)) return "–";
        var a = Math.Abs(x);
        return Num(x, a >= 10_000 ? 1 : a >= 100 ? 2 : a >= 1 ? 4 : 6);
    }

    public static string Qty(double? v)
    {
        if (v is not double x || !double.IsFinite(x)) return "–";
        var a = Math.Abs(x);
        return Num(x, a >= 1000 ? 1 : a >= 1 ? 3 : 5);
    }

    /// grouped, d decimals, in the user's locale
    public static string Num(double v, int d) => v.ToString("N" + d, CultureInfo.CurrentCulture);
    public static string Usd(double? v, int d = 2) => v is double x ? Num(x, d) : "–";

    public static string Big(double? v)
    {
        if (v is not double x) return "–";
        var a = Math.Abs(x);
        if (a >= 1e9) return F(x / 1e9, 2) + "B";
        if (a >= 1e6) return F(x / 1e6, 2) + "M";
        if (a >= 1e3) return F(x / 1e3, 1) + "K";
        return F(x, 2);
    }

    public static string Signed(double? v, int d = 2)
    {
        if (v is not double x) return "–";
        // anything that rounds to zero prints as an unsigned zero, never "+-0.00"
        if (Math.Abs(x) < 0.5 * Math.Pow(10, -d)) return Num(0, d);
        return (x > 0 ? "+" : "") + Num(x, d);
    }

    /// printf "%.Nf" / "%+.Nf" (invariant, no grouping)
    public static string F(double v, int d, bool sign = false)
    {
        v += 0.0; // -0 prints as 0
        return (sign && v >= 0 ? "+" : "") + v.ToString("F" + d, Inv);
    }

    /// "MM-dd HH:mm:ss" in the display time zone
    public static string Time(long ms) => T1.Local(DateTimeOffset.FromUnixTimeMilliseconds(ms)).ToString("MM-dd HH:mm:ss", Inv);
    /// seconds as hh:mm:ss
    public static string Hms(long s) => $"{s / 3600:00}:{s % 3600 / 60:00}:{s % 60:00}";
}

/// Small builders shared by the code-only views.
static class Ui
{
    public static TextBlock Text(string s, double size = 13, Color? fg = null, bool num = false, bool semibold = false)
    {
        var t = new TextBlock { Text = s, FontSize = size, VerticalAlignment = VerticalAlignment.Center, TextTrimming = TextTrimming.CharacterEllipsis };
        if (fg is Color c) t.Foreground = T1.B(c);
        if (semibold) t.FontWeight = FontWeights.SemiBold;
        if (num) Tabular(t);
        return t;
    }

    /// Fixed-width digits for columns of numbers (never a monospaced font).
    public static T Tabular<T>(T e) where T : DependencyObject { Typography.SetNumeralAlignment(e, FontNumeralAlignment.Tabular); return e; }

    /// Foreground: a palette color, or null for the theme's default text color.
    public static void Fg(TextBlock t, Color? c) { if (c is Color x) t.Foreground = T1.B(x); else t.ClearValue(TextBlock.ForegroundProperty); }

    public static StackPanel H(double spacing, params UIElement[] kids)
    {
        var p = new StackPanel { Orientation = Orientation.Horizontal, Spacing = spacing };
        foreach (var k in kids) p.Children.Add(k);
        return p;
    }

    public static StackPanel V(double spacing, params UIElement[] kids)
    {
        var p = new StackPanel { Spacing = spacing };
        foreach (var k in kids) p.Children.Add(k);
        return p;
    }

    /// left content, right content, space between
    public static Grid Spread(FrameworkElement left, FrameworkElement right)
    {
        var g = new Grid { ColumnSpacing = 8 };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        g.Children.Add(left);
        Grid.SetColumn(right, 1);
        g.Children.Add(right);
        return g;
    }

    public static T Tip<T>(T o, string? tip) where T : DependencyObject { ToolTipService.SetToolTip(o, tip); return o; }
    public static FontIcon Icon(string glyph, double size = 14) => new() { Glyph = glyph, FontSize = size };

    /// Compact button / toggle sizing for toolbars.
    public static T Small<T>(T c) where T : Control
    {
        c.FontSize = 12;
        c.Padding = new Thickness(8, 3, 8, 4);
        c.MinHeight = 0;
        c.MinWidth = 0;
        c.VerticalAlignment = VerticalAlignment.Center;
        return c;
    }

    /// Plain text-like button (status bar, list rows).
    public static Button Flat(object content)
    {
        var b = new Button { Content = content, Background = T1.Clear, BorderThickness = new Thickness(0), Padding = new Thickness(4, 2, 4, 2), MinHeight = 0, MinWidth = 0 };
        return b;
    }

    /// Segmented control: adjacent toggle buttons, one checked. pick runs on user clicks only
    /// (programmatic IsChecked changes raise no Click), so rebuilding never sends a call.
    public static Grid Segmented<T>(IEnumerable<(T Id, string Label)> items, T selected, Action<T> pick, bool stretch = false)
    {
        var list = items.ToList();
        var g = new Grid { VerticalAlignment = VerticalAlignment.Center };
        var buttons = new List<ToggleButton>();
        for (int i = 0; i < list.Count; i++)
        {
            var (id, label) = list[i];
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = stretch ? new GridLength(1, GridUnitType.Star) : GridLength.Auto });
            bool first = i == 0, last = i == list.Count - 1;
            var b = Small(new ToggleButton
            {
                Content = label, IsChecked = EqualityComparer<T>.Default.Equals(id, selected),
                CornerRadius = new CornerRadius(first ? 4 : 0, last ? 4 : 0, last ? 4 : 0, first ? 4 : 0),
                HorizontalAlignment = stretch ? HorizontalAlignment.Stretch : HorizontalAlignment.Left,
            });
            b.Padding = new Thickness(10, 3, 10, 4);
            b.Click += (_, _) =>
            {
                foreach (var o in buttons) o.IsChecked = o == b;
                pick(id);
            };
            Grid.SetColumn(b, i);
            buttons.Add(b);
            g.Children.Add(b);
        }
        return g;
    }

    /// Checkable flyout item.
    public static ToggleMenuFlyoutItem Check(string text, bool on, Action a)
    {
        var it = new ToggleMenuFlyoutItem { Text = text, IsChecked = on };
        it.Click += (_, _) => a();
        return it;
    }

    public static MenuFlyoutItem Header(string text) => new() { Text = text, IsEnabled = false };

    public static Border Divider() => new() { Width = 1, Height = 12, Background = T1.B(T1.Line), Margin = new Thickness(6, 0, 6, 0), VerticalAlignment = VerticalAlignment.Center };

    public static bool IsDown(VirtualKey k) => InputKeyboardSource.GetKeyStateForCurrentThread(k).HasFlag(CoreVirtualKeyStates.Down);

    /// Keyboard modifiers for the FFI: 1 shift, 2 ctrl, 4 alt.
    public static uint Mods() =>
        (IsDown(VirtualKey.Shift) ? 1u : 0) | (IsDown(VirtualKey.Control) ? 2u : 0) | (IsDown(VirtualKey.Menu) ? 4u : 0);
}
