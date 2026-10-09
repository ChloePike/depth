using System.Diagnostics;
using System.Globalization;
using System.Reflection;
using System.Text.Json;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Windows.UI;
using Ellipse = Microsoft.UI.Xaml.Shapes.Ellipse;
using VirtualKey = Windows.System.VirtualKey;
using static Depth.I18n;

namespace Depth;

/// Settings dialog (port of SettingsView.swift): sidebar of panes, form on the right. Every control
/// writes straight through to the engine (set_prefs / set_route with the full object); no Save.
/// The pane rebuilds when its inputs change in the state, never while a text field or slider has focus.
sealed class SettingsView : Grid
{
    static readonly (string Id, string Title, string Glyph, uint Color)[] Panes =
    {
        ("general", "General", "", 0x8e8e93), ("appearance", "Appearance", "", 0x5e5ce6),
        ("trading", "Trading", "", 0x30d158), ("keys", "API Keys", "", 0xff9f0a), ("about", "About", "", 0x0a84ff),
    };

    string pane;
    string sig = "";
    bool queued;
    ContentDialog? dialog;
    readonly StackPanel nav = new() { Spacing = 2, Padding = new Thickness(0, 0, 12, 0) };
    readonly TextBlock title = new() { FontSize = 20, FontWeight = FontWeights.SemiBold, Margin = new Thickness(4, 0, 0, 10) };
    readonly ScrollViewer body = new() { Padding = new Thickness(0, 0, 12, 0) };
    readonly SettingsKeys keys = new();
    long? cacheBytes;
    TextBlock? cacheText;

    /// Open the dialog on a pane: "general" | "appearance" | "trading" | "keys" | "about".
    public static async Task Show(string tab)
    {
        var v = new SettingsView(tab);
        // Enter commits a number field; it must not also close the dialog
        var d = new ContentDialog { Content = v, CloseButtonText = L("Done"), DefaultButton = ContentDialogButton.None };
        d.Resources["ContentDialogMaxWidth"] = 1000.0;
        d.Resources["ContentDialogMaxHeight"] = 900.0;
        v.dialog = d;
        Store.Shared.Changed += v.OnChanged;
        try { await ModalDialog.Show(d); }
        finally { Store.Shared.Changed -= v.OnChanged; v.keys.Close(); }
    }

    SettingsView(string tab)
    {
        pane = Panes.Any(p => p.Id == tab) ? tab : "general";
        Width = 800;
        Height = 540;
        ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(190) });
        ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        Children.Add(nav);
        var right = new Grid();
        right.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        right.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
        right.Children.Add(title);
        Grid.SetRow(body, 1);
        right.Children.Add(body);
        Grid.SetColumn(right, 1);
        Children.Add(right);
        Build();
    }

    // deferred: Changed is raised from inside Store.Call (a control's own write) and rebuilding there
    // would tear down the control that is still handling its event
    void OnChanged()
    {
        if (queued) return;
        queued = true;
        DispatcherQueue.TryEnqueue(() => { queued = false; Sync(); });
    }

    void Sync()
    {
        if (Signature() == sig || Typing()) return;
        Build();
    }

    /// Everything the panes show; a change rebuilds the current pane.
    static string Signature()
    {
        var s = Store.Shared.State;
        return JsonSerializer.Serialize(new object[] { s.Lang, s.Langs, s.Prefs, s.Route, s.Keys, s.Tradable, s.Trade.Venue, T1.IsDark }, Json.Opts);
    }

    bool Typing() => XamlRoot != null && FocusManager.GetFocusedElement(XamlRoot) is TextBox or PasswordBox or Slider;

    public void Build()
    {
        sig = Signature();
        if (dialog != null) dialog.RequestedTheme = ModalDialog.Theme;
        nav.Children.Clear();
        foreach (var (id, t, glyph, col) in Panes)
        {
            var icon = Ui.Icon(glyph, 11);
            icon.Foreground = T1.B(Color.FromArgb(255, 255, 255, 255));
            var sq = new Border { Width = 22, Height = 22, CornerRadius = new CornerRadius(6), Background = T1.B(Color.FromArgb(255, (byte)(col >> 16), (byte)(col >> 8), (byte)col)), Child = icon };
            var b = Ui.Flat(Ui.H(10, sq, Ui.Text(L(t), 13)));
            b.HorizontalAlignment = HorizontalAlignment.Stretch;
            b.HorizontalContentAlignment = HorizontalAlignment.Left;
            b.Padding = new Thickness(8, 5, 8, 5);
            b.CornerRadius = new CornerRadius(6);
            if (id == pane) b.Background = T1.B(T1.Hl);
            b.Click += (_, _) => { if (pane == id) return; if (id != "keys") keys.Close(); pane = id; Build(); };
            nav.Children.Add(b);
        }
        title.Text = L(Panes.First(p => p.Id == pane).Title);
        body.Content = pane switch
        {
            "appearance" => Appearance(),
            "trading" => Trading(),
            "keys" => keys.Build(Build),
            "about" => About(),
            _ => General(),
        };
    }

    // MARK: General

    static readonly int[] Tzs = { -600, -540, -480, -420, -360, -300, -240, -180, -120, -60, 0, 60, 120, 180, 210, 240, 270, 300, 330, 345, 360, 390, 420, 480, 525, 540, 570, 600, 660, 720 };

    sealed class BytesReply { public long? Bytes { get; set; } }

    FrameworkElement General()
    {
        var s = Store.Shared.State;
        var lang = SettingsUi.Combo(s.Langs.Select(l => l.Count > 1 ? l[1] : l[0]), s.Langs.FindIndex(l => l.Count > 0 && l[0] == s.Lang),
            i => Store.Shared.Call("set_lang", new() { ["lang"] = s.Langs[i][0] }));
        var sys = L("System") + " (" + T1.TzLabel(TimeZoneInfo.Local.GetUtcOffset(DateTimeOffset.UtcNow)) + ")";
        var tz = SettingsUi.Combo(new[] { sys }.Concat(Tzs.Select(m => T1.TzLabel(TimeSpan.FromMinutes(m)))),
            s.Prefs.TzMin is int cur ? (Array.IndexOf(Tzs, cur) is int k && k >= 0 ? k + 1 : -1) : 0,
            i => SettingsUi.SetPrefs(p => p.TzMin = i == 0 ? null : Tzs[i - 1]));

        cacheBytes ??= Store.Shared.Query<BytesReply>("cache_size")?.Bytes;
        cacheText = Ui.Text(Mb(cacheBytes), 12, T1.Mu, num: true);
        var clear = Ui.Small(new Button { Content = L("Clear Cache") });
        clear.Click += (_, _) =>
        {
            try { cacheBytes = Store.Shared.Call("clear_cache")?["bytes"]?.GetValue<long>() ?? cacheBytes; } catch (Exception) { }
            if (cacheText != null) cacheText.Text = Mb(cacheBytes);
        };

        return Ui.V(18,
            SettingsUi.Section(null, L("Interface language. Numbers, symbols and codes are not translated."), SettingsUi.Row(L("Language"), lang)),
            SettingsUi.Section(null, L("Chart axis, tables and the status bar clock."), SettingsUi.Row(L("Time zone"), tz)),
            SettingsUi.Section(L("Storage"), L("Chart history, coin logos and crash logs. History not viewed for 30 days is removed automatically."),
                SettingsUi.Row(L("Cache and logs"), Ui.H(10, cacheText, clear))));
    }

    static string Mb(long? b) => b is long x ? (x / 1e6).ToString("F1", CultureInfo.InvariantCulture) + " MB" : "–";

    // MARK: Appearance

    static readonly (string Name, int[] Rgb)[] Accents =
    {
        ("Blue", new[] { 76, 158, 235 }), ("Gold", new[] { 240, 185, 11 }), ("Violet", new[] { 155, 140, 240 }),
        ("Teal", new[] { 46, 196, 182 }), ("Orange", new[] { 245, 138, 61 }), ("Pink", new[] { 224, 91, 196 }),
    };

    static Color Rgb(IList<int> c) => c.Count == 3 ? Color.FromArgb(255, (byte)c[0], (byte)c[1], (byte)c[2]) : T1.Accent;

    FrameworkElement Appearance()
    {
        var p = Store.Shared.State.Prefs;
        var d = new Prefs();
        var theme = Ui.Segmented(new[] { ("system", L("System")), ("dark", L("Dark")), ("light", L("Light")) }, p.Theme, t => SettingsUi.SetPrefs(q => q.Theme = t));

        var accents = Ui.H(6);
        foreach (var (name, rgb) in Accents)
        {
            bool on = p.Accent.SequenceEqual(rgb);
            var dot = new Ellipse { Width = 14, Height = 14, Fill = T1.B(Rgb(rgb)) };
            var ring = new Grid { Width = 22, Height = 22 };
            ring.Children.Add(new Ellipse { Stroke = on ? T1.B(T1.Fg) : T1.Clear, StrokeThickness = 2 });
            dot.HorizontalAlignment = HorizontalAlignment.Center;
            dot.VerticalAlignment = VerticalAlignment.Center;
            ring.Children.Add(dot);
            var b = Ui.Flat(ring);
            b.Padding = new Thickness(0);
            var copy = rgb.ToList();
            b.Click += (_, _) => SettingsUi.SetPrefs(q => q.Accent = copy);
            AutomationProperties.SetName(b, L(name));
            accents.Children.Add(Ui.Tip(b, L(name)));
        }
        // custom: the system color picker in a flyout, written once when it closes
        var picker = new ColorPicker { Color = Rgb(p.Accent), IsAlphaEnabled = false, IsMoreButtonVisible = false };
        var fly = new Flyout { Content = picker };
        fly.Closed += (_, _) =>
        {
            var c = picker.Color;
            var v = new List<int> { c.R, c.G, c.B };
            if (!Store.Shared.State.Prefs.Accent.SequenceEqual(v)) SettingsUi.SetPrefs(q => q.Accent = v);
        };
        var custom = Ui.Flat(Ui.Icon("", 14));
        custom.Flyout = fly;
        accents.Children.Add(Ui.Tip(custom, L("Custom")));

        var colors = SettingsUi.Combo(new[] { L("Green up, red down"), L("Red up, green down") }, p.RedUp ? 1 : 0, i => SettingsUi.SetPrefs(q => q.RedUp = i == 1));

        var zoomLabel = Ui.Text($"{(int)Math.Round(p.Zoom * 100)}%", 12, T1.Mu, num: true);
        zoomLabel.Width = 44;
        zoomLabel.TextAlignment = TextAlignment.Right;
        var zoom = new Slider { Minimum = 0.85, Maximum = 1.3, StepFrequency = 0.05, Width = 220, Value = p.Zoom, VerticalAlignment = VerticalAlignment.Center };
        zoom.ValueChanged += (_, e) =>
        {
            var v = Math.Round(e.NewValue, 2);
            zoomLabel.Text = $"{(int)Math.Round(v * 100)}%";
            if (Math.Abs(v - Store.Shared.State.Prefs.Zoom) > 1e-6) SettingsUi.SetPrefs(q => q.Zoom = v);
        };
        var radiusLabel = Ui.Text($"{p.Radius} pt", 12, T1.Mu, num: true);
        radiusLabel.Width = 44;
        radiusLabel.TextAlignment = TextAlignment.Right;
        var radius = new Slider { Minimum = 0, Maximum = 12, StepFrequency = 1, Width = 220, Value = p.Radius, VerticalAlignment = VerticalAlignment.Center };
        radius.ValueChanged += (_, e) =>
        {
            int v = (int)Math.Round(e.NewValue);
            radiusLabel.Text = $"{v} pt";
            if (v != Store.Shared.State.Prefs.Radius) SettingsUi.SetPrefs(q => q.Radius = v);
        };

        var restore = new Button { Content = L("Restore Default Appearance") };
        restore.IsEnabled = !(p.Theme == d.Theme && p.Accent.SequenceEqual(d.Accent) && !p.RedUp && p.Zoom == 1 && p.Radius == d.Radius);
        restore.Click += (_, _) => SettingsUi.SetPrefs(q => { q.Theme = d.Theme; q.Accent = d.Accent.ToList(); q.RedUp = d.RedUp; q.Zoom = d.Zoom; q.Radius = d.Radius; });

        return Ui.V(18,
            SettingsUi.Section(L("Colors"), L("The accent marks selection and highlights in the chart and book. Price colors apply to candles, the order book, PnL and buy/sell buttons."),
                SettingsUi.Row(L("Theme"), theme), SettingsUi.Row(L("Accent color"), accents), SettingsUi.Row(L("Price colors"), colors)),
            SettingsUi.Section(L("Chart and book"), L("Scales the chart, order book and option chain. The rest of the window follows the system text size."),
                SettingsUi.Row(L("Interface size"), Ui.H(6, zoom, zoomLabel)), SettingsUi.Row(L("Corner radius"), Ui.H(6, radius, radiusLabel))),
            SettingsUi.Section(null, null, restore));
    }

    // MARK: Trading

    static readonly double[] DefaultFees = { 0.055, 0.02 };

    FrameworkElement Trading()
    {
        var s = Store.Shared.State;
        var r = s.Route;
        var routing = Ui.Segmented(new[] { (true, L("Smart")), (false, L("Fixed venue")) }, r.Smart, v => SettingsUi.SetRoute(q => q.Smart = v));
        var venues = s.Tradable.ToList();
        var venue = SettingsUi.Combo(venues.Select(ex => s.Keys.FirstOrDefault(k => k.Ex == ex)?.Verified ?? true ? ex : $"{ex} ({L("untested")})"),
            venues.IndexOf(s.Trade.Venue), i => Store.Shared.Call("set_trade_venue", new() { ["ex"] = venues[i] }));

        bool smart = r.Smart, split = r.Smart && r.AllowSplit;
        var allow = new ToggleSwitch { IsOn = r.AllowSplit, OnContent = "", OffContent = "", MinWidth = 0, IsEnabled = smart };
        allow.Toggled += (_, _) => { if (allow.IsOn != Store.Shared.State.Route.AllowSplit) SettingsUi.SetRoute(q => q.AllowSplit = allow.IsOn); };
        var legs = Ui.H(8);
        var minus = Ui.Small(new Button { Content = "−", IsEnabled = split && r.MaxLegs > 1 });
        var plus = Ui.Small(new Button { Content = "+", IsEnabled = split && r.MaxLegs < 4 });
        minus.Click += (_, _) => SettingsUi.SetRoute(q => q.MaxLegs = Math.Max(1, q.MaxLegs - 1));
        plus.Click += (_, _) => SettingsUi.SetRoute(q => q.MaxLegs = Math.Min(4, q.MaxLegs + 1));
        legs.Children.Add(Ui.Text($"{r.MaxLegs}", 12, num: true));
        legs.Children.Add(minus);
        legs.Children.Add(plus);

        FrameworkElement Num(string label, double v, string unit, bool on, Action<RoutePolicy, double> set, int digits = 2) =>
            SettingsUi.Row(label, SettingsUi.NumField(v, digits, double.MinValue, double.MaxValue, unit, on, x => SettingsUi.SetRoute(q => set(q, x))));

        var confirm = new ToggleSwitch { IsOn = s.Prefs.Confirm, OnContent = "", OffContent = "", MinWidth = 0 };
        confirm.Toggled += (_, _) => { if (confirm.IsOn != Store.Shared.State.Prefs.Confirm) SettingsUi.SetPrefs(q => q.Confirm = confirm.IsOn); };

        var fees = venues.Select(ex => (FrameworkElement)SettingsUi.Row(Ui.H(8, T1.VenueIcon(ex, 16), Ui.Text(ex, 13)), FeeRow(ex))).ToArray();

        return Ui.V(18,
            SettingsUi.Section(L("Order routing"), r.Smart
                    ? L("Smart routing picks venues by fee-adjusted price from each venue's own book and can split large orders. The preferred venue drives the order panel's book and limits.")
                    : L("Every order goes to the trading venue."),
                SettingsUi.Row(L("Routing"), routing), SettingsUi.Row(L(r.Smart ? "Preferred venue" : "Trading venue"), venue)),
            SettingsUi.Section(L("Smart routing"), L("Each leg is priced on its venue's real bid/ask, never the composite index. Venues whose book is stale or too far from the others are skipped."),
                SettingsUi.Row(L("Allow split orders"), allow),
                SettingsUi.Row(L("Maximum legs"), legs),
                Num(L("Split when saving at least"), r.SplitBps, "bp", split, (q, x) => q.SplitBps = x),
                Num(L("Minimum notional to split"), r.MinSplitNotional, "USDT", split, (q, x) => q.MinSplitNotional = x),
                Num(L("Maximum slippage"), r.MaxSlipBps, "bp", smart, (q, x) => q.MaxSlipBps = x),
                Num(L("Maximum venue dispersion"), r.MaxDispBps, "bp", smart, (q, x) => q.MaxDispBps = x),
                Num(L("Ignore books older than"), r.MaxStaleMs, "ms", smart, (q, x) => q.MaxStaleMs = (long)Math.Round(x), 0)),
            SettingsUi.Section(L("Orders"), L("Shows an order summary before it is sent."), SettingsUi.Row(L("Confirm before sending orders"), confirm)),
            SettingsUi.Section(L("Fees (your tier)"), L("Taker / maker in percent per fill. Used for fee estimates and routing; enter your VIP tier's rates."), fees));
    }

    static FrameworkElement FeeRow(string ex)
    {
        var cur = Store.Shared.State.Prefs.Fees.GetValueOrDefault(ex) ?? DefaultFees;
        FrameworkElement Field(int i, string tip) => Ui.Tip(SettingsUi.NumField(i < cur.Length ? cur[i] : 0, 4, -0.05, 0.2, null, true, v => SettingsUi.SetPrefs(p =>
        {
            var f = (p.Fees.GetValueOrDefault(ex) ?? DefaultFees).ToList();
            while (f.Count < 2) f.Add(0);
            f[i] = v;
            p.Fees[ex] = f.ToArray();
        }), 64), tip);
        return Ui.H(6, Field(0, L("Taker")), Ui.Text("/", 12, T1.Dim), Field(1, L("Maker")), Ui.Text("%", 12, T1.Mu));
    }

    // MARK: About

    static FrameworkElement About()
    {
        var ver = Assembly.GetEntryAssembly()?.GetCustomAttribute<AssemblyInformationalVersionAttribute>()?.InformationalVersion?.Split('+')[0] ?? "0.1.0";
        var name = Ui.Text("Depth", 18, semibold: true);
        var head = Ui.V(3, name, Ui.Text($"{L("Version")} {ver}", 12, T1.Mu, num: true));
        head.Margin = new Thickness(0, 4, 0, 4);
        string data = System.IO.Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "TerminalOne");
        string cache = System.IO.Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "TerminalOne", "Cache");
        return Ui.V(18,
            SettingsUi.Section(null, L("Local multi-exchange trading terminal. Runs entirely on this PC: no relay, no server."), head),
            SettingsUi.Section(L("Data"), null,
                PathRow(L("Settings"), data), PathRow(L("Cache"), cache),
                SettingsUi.Row(L("API keys"), Ui.Text(L("Windows Credential Manager (terminal-one)"), 12, T1.Mu))));
    }

    static FrameworkElement PathRow(string label, string path)
    {
        var t = Ui.Text(path, 12, T1.Mu);
        t.IsTextSelectionEnabled = true;
        t.MaxWidth = 380;
        var open = Ui.Flat(Ui.Icon("", 13));
        open.IsEnabled = Directory.Exists(path);
        open.Click += (_, _) => { try { Process.Start(new ProcessStartInfo("explorer.exe", $"\"{path}\"") { UseShellExecute = true }); } catch (Exception) { } };
        return SettingsUi.Row(label, Ui.H(6, t, Ui.Tip(open, L("Show in Explorer"))));
    }
}

/// Form building blocks and write-through helpers shared by the settings panes.
static class SettingsUi
{
    static T Clone<T>(T v) => JsonSerializer.Deserialize<T>(JsonSerializer.Serialize(v, Json.Opts), Json.Opts)!;

    /// Change one field of the current prefs and write the whole object.
    public static void SetPrefs(Action<Prefs> f) { var p = Clone(Store.Shared.State.Prefs); f(p); Store.Shared.SetPrefs(p); }
    public static void SetRoute(Action<RoutePolicy> f) { var r = Clone(Store.Shared.State.Route); f(r); Store.Shared.SetRoute(r); }

    /// Header caption, a card of rows separated by hairlines, footer explanation.
    public static StackPanel Section(string? header, string? footer, params FrameworkElement[] rows)
    {
        var p = new StackPanel { Spacing = 6 };
        if (header != null) p.Children.Add(Ui.Text(header, 12, T1.Mu, semibold: true));
        var card = new StackPanel();
        for (int i = 0; i < rows.Length; i++)
        {
            if (i > 0) card.Children.Add(new Border { Height = 1, Background = T1.B(T1.Line), Margin = new Thickness(0, 6, 0, 6) });
            card.Children.Add(rows[i]);
        }
        p.Children.Add(new Border { Child = card, Background = T1.B(T1.Panel), CornerRadius = new CornerRadius(8), Padding = new Thickness(12, 10, 12, 10) });
        if (footer != null)
        {
            var f = Ui.Text(footer, 11, T1.Mu);
            f.TextWrapping = TextWrapping.Wrap;
            f.TextTrimming = TextTrimming.None;
            f.Margin = new Thickness(4, 0, 4, 0);
            p.Children.Add(f);
        }
        return p;
    }

    public static Grid Row(string label, FrameworkElement control) => Row(Ui.Text(label, 13), control);

    public static Grid Row(FrameworkElement label, FrameworkElement control)
    {
        var g = Ui.Spread(label, control);
        g.MinHeight = 30;
        control.VerticalAlignment = VerticalAlignment.Center;
        return g;
    }

    /// Picker; pick runs on user selection only.
    public static ComboBox Combo(IEnumerable<string> items, int selected, Action<int> pick)
    {
        var c = new ComboBox { MinWidth = 200, FontSize = 12 };
        foreach (var i in items) c.Items.Add(i);
        c.SelectedIndex = selected;
        c.SelectionChanged += (_, _) => { if (c.SelectedIndex >= 0) pick(c.SelectedIndex); };
        return c;
    }

    /// Number field with an optional unit suffix, committed on Enter or focus loss (invariant decimal
    /// point, grouping commas ignored); invalid text reverts, values clamp to [min, max].
    public static FrameworkElement NumField(double value, int digits, double min, double max, string? unit, bool enabled, Action<double> commit, double width = 80)
    {
        string F(double v) => v.ToString(digits == 0 ? "0" : "0." + new string('#', digits), CultureInfo.InvariantCulture);
        var t = Ui.Tabular(new TextBox { Text = F(value), Width = width, TextAlignment = TextAlignment.Right, IsEnabled = enabled, FontSize = 12, MinHeight = 0, Padding = new Thickness(6, 4, 6, 4) });
        double cur = value;
        void Commit()
        {
            if (AccountCells.Parse(t.Text) is not double v) { t.Text = F(cur); return; }
            v = Math.Clamp(Math.Round(v, digits), min, max);
            t.Text = F(v);
            if (v == cur) return;
            cur = v;
            commit(v);
        }
        t.LostFocus += (_, _) => Commit();
        t.KeyDown += (_, e) => { if (e.Key == VirtualKey.Enter) { Commit(); e.Handled = true; } };
        if (unit == null) return t;
        var u = Ui.Text(unit, 12, T1.Mu);
        u.Width = 36;
        return Ui.H(6, t, u);
    }
}
