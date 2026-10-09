using System.Runtime.InteropServices;
using Microsoft.UI;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Input;
using Microsoft.UI.Text;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Windows.Graphics;
using Windows.UI;
using Colors = Microsoft.UI.Colors;
using VirtualKey = Windows.System.VirtualKey;
using static Depth.I18n;

namespace Depth;

/// The window (port of App.swift + RootView, TopBar.swift's toolbar): custom title bar with the pair
/// picker, composite price, market mode, market clock, order panel and settings buttons; the stats
/// strip; chart + book (egui) beside the order panel; the account strip below; the status bar.
sealed class MainWindow : Window
{
    public static MainWindow Current { get; private set; } = null!;

    /// Order entry column (Perp only, toggled with Ctrl+Shift+I); the order panel port sets Child.
    public readonly Border OrderPanelHost = new() { Width = 300 };
    /// Account tables below the chart, full width; the account panel port sets Child.
    public readonly Border AccountPanelHost = new() { CornerRadius = new CornerRadius(8), BorderThickness = new Thickness(1), Margin = new Thickness(8, 0, 8, 0) };

    readonly Grid root = new();
    readonly Grid titleRow = new() { Height = 48 };
    readonly EguiPanel egui = new() { Margin = new Thickness(6, 0, 6, 6) };
    readonly TopStats stats = new();
    readonly ChartBar chartBar = new();
    readonly BookPanel book = new() { Width = 300, HorizontalAlignment = HorizontalAlignment.Right, Margin = new Thickness(0, 0, 6, 0) };
    readonly StatusBar status = new();
    readonly MarketClock clock = new();
    readonly Border chartCard = new() { CornerRadius = new CornerRadius(8), BorderThickness = new Thickness(1) };
    readonly Button pairBtn = Ui.Flat(null!), orderBtn = Ui.Flat(Ui.Icon("", 15)), gearBtn = Ui.Flat(Ui.Icon("", 15));
    readonly Grid modeHost = new() { VerticalAlignment = VerticalAlignment.Center };
    readonly TextBlock price = Ui.Text("–", 18, num: true, semibold: true), arrow = Ui.Text("↑", 12, semibold: true);
    readonly Border priceBox = new() { Width = 150, CornerRadius = new CornerRadius(6), Padding = new Thickness(6, 2, 6, 2), VerticalAlignment = VerticalAlignment.Center };
    readonly ColumnDefinition leftInset = new() { Width = new GridLength(0) }, rightInset = new() { Width = new GridLength(140) };
    readonly RowDefinition accountRow = new();
    readonly Flyout pickerFly = new() { Placement = FlyoutPlacementMode.BottomEdgeAlignedLeft };
    readonly DispatcherQueueTimer second, flash;
    SymbolPicker? picker;
    bool orderPanel = HostPrefs.Get("orderPanel", true);
    double accountH = HostPrefs.Get("accountHeight", 250.0);
    double? lastPrice;
    int dir;
    bool settingsOpen;

    public MainWindow()
    {
        Current = this;
        Title = "Depth";
        ExtendsContentIntoTitleBar = true;
        SystemBackdrop = new MicaBackdrop();
        BuildLayout();
        MainWindow.Current.OrderPanelHost.Child = new OrderPanel();
        AccountPanelHost.Child = new AccountPanel();
        Content = root;
        SetTitleBar(titleRow);
        var tb = AppWindow.TitleBar;
        tb.PreferredHeightOption = TitleBarHeightOption.Tall;
        tb.ButtonBackgroundColor = Colors.Transparent;
        tb.ButtonInactiveBackgroundColor = Colors.Transparent;
        SizeWindow();

        root.Loaded += (_, _) => { ApplyTheme(); ThemeChanged(); };
        root.ActualThemeChanged += (_, _) => ThemeChanged();
        // the drag region must follow the title bar controls as they resize (pair name, price, clock text)
        foreach (var e in new FrameworkElement[] { root, pairBtn, modeHost, clock.View })
            e.SizeChanged += (_, _) => UpdatePassthrough();
        root.PreviewKeyDown += OnShortcut;
        Store.Shared.Changed += OnChanged;

        second = DispatcherQueue.CreateTimer();
        second.Interval = TimeSpan.FromSeconds(1);
        second.Tick += (_, _) => { stats.Tick(); status.Tick(); clock.Tick(); UpdatePassthrough(); };
        second.Start();
        flash = DispatcherQueue.CreateTimer();
        flash.Interval = TimeSpan.FromMilliseconds(450);
        flash.IsRepeating = false;
        flash.Tick += (_, _) => priceBox.Background = null;

        Closed += (_, _) => { second.Stop(); Store.Shared.Changed -= OnChanged; egui.Dispose(); };
        OnChanged();
        status.Tick();
        clock.Tick();
    }

    void BuildLayout()
    {
        foreach (var h in new[] { new GridLength(48), GridLength.Auto, new GridLength(1, GridUnitType.Star), GridLength.Auto })
            root.RowDefinitions.Add(new RowDefinition { Height = h });
        accountRow.Height = new GridLength(accountH);
        root.RowDefinitions.Add(accountRow);
        root.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        root.RowDefinitions[2].MinHeight = 280;

        // title bar: the row is the drag region; the controls in it are passthrough rects (UpdatePassthrough)
        foreach (var c in new[] { leftInset, new ColumnDefinition { Width = GridLength.Auto }, new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) },
                     new ColumnDefinition { Width = GridLength.Auto }, new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) }, new ColumnDefinition { Width = GridLength.Auto }, rightInset })
            titleRow.ColumnDefinitions.Add(c);
        pairBtn.Padding = new Thickness(8, 4, 8, 4);
        pairBtn.Click += (_, _) => OpenPicker();
        pairBtn.VerticalAlignment = VerticalAlignment.Center;
        pickerFly.FlyoutPresenterStyle = PresenterStyle();
        pickerFly.Opened += (_, _) => picker?.Start();
        pickerFly.Closed += (_, _) => picker?.Stop();
        priceBox.Child = Ui.H(3, price, arrow);
        var left = Ui.H(8, pairBtn, priceBox);
        left.Margin = new Thickness(8, 0, 0, 0);
        Grid.SetColumn(left, 1);
        Grid.SetColumn(modeHost, 3);
        orderBtn.Click += (_, _) => ToggleOrderPanel();
        gearBtn.Click += (_, _) => OpenSettings("general");
        foreach (var b in new[] { orderBtn, gearBtn }) { b.Padding = new Thickness(8, 6, 8, 6); b.VerticalAlignment = VerticalAlignment.Center; }
        var right = Ui.H(4, clock.View, orderBtn, gearBtn);
        right.Margin = new Thickness(0, 0, 8, 0);
        Grid.SetColumn(right, 5);
        titleRow.Children.Add(left);
        titleRow.Children.Add(modeHost);
        titleRow.Children.Add(right);
        root.Children.Add(titleRow);

        Grid.SetRow(stats, 1);
        root.Children.Add(stats);

        // chart + book and the order panel; egui draws the chart and the book rows (a 300 DIP column on
        // the right), the native book header sits on top of that column
        var inner = new Grid();
        inner.Children.Add(egui);
        inner.Children.Add(book);
        var card = new Grid();
        card.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        card.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
        card.Children.Add(chartBar);
        Grid.SetRow(inner, 1);
        card.Children.Add(inner);
        chartCard.Child = card;
        var main = new Grid { ColumnSpacing = 8, Margin = new Thickness(8, 0, 8, 0) };
        main.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        main.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        main.Children.Add(chartCard);
        Grid.SetColumn(OrderPanelHost, 1);
        main.Children.Add(OrderPanelHost);
        Grid.SetRow(main, 2);
        root.Children.Add(main);

        var split = new SplitHandle(() => accountH, h => { accountH = h; accountRow.Height = new GridLength(h); }, () => HostPrefs.Set("accountHeight", accountH));
        Grid.SetRow(split, 3);
        root.Children.Add(split);
        Grid.SetRow(AccountPanelHost, 4);
        root.Children.Add(AccountPanelHost);
        Grid.SetRow(status, 5);
        root.Children.Add(status);
    }

    /// The picker is 480 wide; the default flyout presenter caps content at 456.
    static Style PresenterStyle()
    {
        var s = new Style(typeof(FlyoutPresenter));
        s.Setters.Add(new Setter(FrameworkElement.MaxWidthProperty, 640.0));
        s.Setters.Add(new Setter(FrameworkElement.MaxHeightProperty, 800.0));
        s.Setters.Add(new Setter(Control.PaddingProperty, new Thickness(12)));
        return s;
    }

    void OnChanged()
    {
        var s = Store.Shared;
        var st = s.State;
        if (s.Dirty("prefs")) ApplyTheme();
        if (s.Dirty("base", "mode", "lang")) BuildTitle();
        if (s.Dirty("header")) UpdatePrice();
        stats.Update();
        chartBar.Update();
        book.Update();
        status.Update();
        bool perp = st.Mode == "Perp";
        OrderPanelHost.Visibility = perp && orderPanel ? Visibility.Visible : Visibility.Collapsed;
        orderBtn.IsEnabled = perp;
        book.Visibility = st.Mode != "Option" ? Visibility.Visible : Visibility.Collapsed;
    }

    void BuildTitle()
    {
        var st = Store.Shared.State;
        pairBtn.Content = Ui.H(6, T1.CoinIcon(st.Base, 18), Ui.Text($"{st.Base}/USDT", 15, semibold: true), Ui.Icon("", 9));
        Ui.Tip(pairBtn, L("Choose pair") + " (Ctrl+K)");
        Ui.Tip(priceBox, L("Composite index (volume-weighted venue mids); reference only, orders use the venue's own book"));
        modeHost.Children.Clear();
        modeHost.Children.Add(Ui.Segmented(Pairs.Modes.Select(m => (m.Id, L(m.Label))), st.Mode, m => Store.Shared.Call("set_mode", new() { ["mode"] = m })));
        Ui.Tip(orderBtn, L("Show or hide the order panel") + " (Ctrl+Shift+I)");
        Ui.Tip(gearBtn, L("Settings") + " (Ctrl+,)");
    }

    /// Composite index price with a direction arrow; flashes on every change.
    void UpdatePrice()
    {
        var p = Store.Shared.State.Header.Price;
        if (lastPrice is double o && p is double n && n != o)
        {
            dir = n > o ? 1 : -1;
            priceBox.Background = T1.B(dir > 0 ? T1.Up : T1.Down, 0.22);
            flash.Stop();
            flash.Start();
        }
        lastPrice = p;
        price.Text = Fmt.Px(p);
        Color? c = dir > 0 ? T1.Up : dir < 0 ? T1.Down : (Color?)null;
        Ui.Fg(price, c);
        Ui.Fg(arrow, c);
        arrow.Text = dir < 0 ? "↓" : "↑";
        arrow.Opacity = dir == 0 ? 0 : 1;
    }

    void ApplyTheme()
    {
        root.RequestedTheme = Store.Shared.State.Prefs.Theme switch { "dark" => ElementTheme.Dark, "light" => ElementTheme.Light, _ => ElementTheme.Default };
    }

    void ThemeChanged()
    {
        T1.IsDark = root.ActualTheme == ElementTheme.Dark;
        var tb = AppWindow.TitleBar;
        tb.ButtonForegroundColor = T1.Fg;
        tb.ButtonHoverForegroundColor = T1.Fg;
        tb.ButtonPressedForegroundColor = T1.Fg;
        tb.ButtonInactiveForegroundColor = T1.Dim;
        tb.ButtonHoverBackgroundColor = T1.Hl;
        tb.ButtonPressedBackgroundColor = T1.Line;
        // content surfaces (chart, tables) are opaque so data keeps full contrast; Mica shows around them
        foreach (var b in new[] { chartCard, AccountPanelHost })
        {
            b.Background = T1.B(T1.Bg);
            b.BorderBrush = T1.B(T1.Line);
        }
        Store.Shared.Refresh();
    }

    /// Title bar controls take clicks; the rest of the row drags the window.
    void UpdatePassthrough()
    {
        if (root.XamlRoot == null) return;
        double k = root.XamlRoot.RasterizationScale;
        leftInset.Width = new GridLength(AppWindow.TitleBar.LeftInset / k);
        rightInset.Width = new GridLength(AppWindow.TitleBar.RightInset / k);
        var rects = new FrameworkElement[] { pairBtn, modeHost, clock.View, orderBtn, gearBtn }.Where(e => e.ActualWidth > 0).Select(e =>
        {
            var r = e.TransformToVisual(null).TransformBounds(new Windows.Foundation.Rect(0, 0, e.ActualWidth, e.ActualHeight));
            return new RectInt32((int)Math.Round(r.X * k), (int)Math.Round(r.Y * k), (int)Math.Round(r.Width * k), (int)Math.Round(r.Height * k));
        }).ToArray();
        InputNonClientPointerSource.GetForWindowId(AppWindow.Id).SetRegionRects(NonClientRegionKind.Passthrough, rects);
    }

    void OpenPicker()
    {
        picker = new SymbolPicker(Store.Shared.State.Base, () => pickerFly.Hide());
        pickerFly.Content = picker;
        pickerFly.ShowAt(pairBtn);
    }

    void ToggleOrderPanel()
    {
        orderPanel = !orderPanel;
        HostPrefs.Set("orderPanel", orderPanel);
        OrderPanelHost.Visibility = Store.Shared.State.Mode == "Perp" && orderPanel ? Visibility.Visible : Visibility.Collapsed;
    }

    /// Settings dialog, opened on a tab: "general" | "appearance" | "trading" | "keys" | "about".
    public async void OpenSettings(string tab = "general")
    {
        if (settingsOpen || root.XamlRoot == null) return;
        settingsOpen = true;
        try { await SettingsView.Show(tab); }
        finally { settingsOpen = false; }
    }

    /// Menu shortcuts of the macOS host, Cmd -> Ctrl: Ctrl+, settings, Ctrl+K pair, Ctrl+[ / ] favorites,
    /// Ctrl+D star, Ctrl+1..4 market, Ctrl+Shift+1..6 interval, Ctrl+Shift+I order panel.
    void OnShortcut(object sender, KeyRoutedEventArgs e)
    {
        uint m = Ui.Mods();
        if ((m & 2) == 0 || (m & 4) != 0) return;
        bool shift = (m & 1) != 0;
        int n = (int)e.Key - (int)VirtualKey.Number1;
        var st = Store.Shared.State;
        switch ((int)e.Key)
        {
            case 188: OpenSettings("general"); break; // VK_OEM_COMMA
            case 219: Pairs.Step(-1); break; // [
            case 221: Pairs.Step(1); break; // ]
            case (int)VirtualKey.K when !shift: OpenPicker(); break;
            case (int)VirtualKey.D when !shift: Pairs.ToggleFavorite(st.Base); break;
            case (int)VirtualKey.I when shift: ToggleOrderPanel(); break;
            default:
                if (!shift && n >= 0 && n < Pairs.Modes.Length) Store.Shared.Call("set_mode", new() { ["mode"] = Pairs.Modes[n].Id });
                else if (shift && n >= 0 && n < Pairs.Intervals.Length) Store.Shared.Call("chart", new() { ["tf"] = Pairs.Intervals[n].Min });
                else return;
                break;
        }
        e.Handled = true;
    }

    [DllImport("user32.dll")] static extern uint GetDpiForWindow(IntPtr hwnd);
    [DllImport("user32.dll")] static extern IntPtr SetWindowLongPtrW(IntPtr hwnd, int index, IntPtr value);
    [DllImport("user32.dll")] static extern IntPtr CallWindowProcW(IntPtr prev, IntPtr hwnd, uint msg, IntPtr w, IntPtr l);
    delegate IntPtr WndProc(IntPtr hwnd, uint msg, IntPtr w, IntPtr l);
    WndProc? proc; // kept alive: native code holds a pointer to it
    IntPtr prevProc;

    /// 1680x1020 DIPs centered on the work area; minimum 1100x700 through WM_GETMINMAXINFO
    /// (OverlappedPresenter.PreferredMinimumWidth needs Windows App SDK 1.7).
    void SizeWindow()
    {
        var hwnd = WinRT.Interop.WindowNative.GetWindowHandle(this);
        proc = (h, msg, w, l) =>
        {
            if (msg == 0x0024) // WM_GETMINMAXINFO: ptMinTrackSize at offset 24
            {
                double s = GetDpiForWindow(h) / 96.0;
                Marshal.WriteInt32(l, 24, (int)(1100 * s));
                Marshal.WriteInt32(l, 28, (int)(700 * s));
            }
            return CallWindowProcW(prevProc, h, msg, w, l);
        };
        prevProc = SetWindowLongPtrW(hwnd, -4, Marshal.GetFunctionPointerForDelegate(proc)); // GWLP_WNDPROC
        double k = GetDpiForWindow(hwnd) / 96.0;
        var wa = DisplayArea.GetFromWindowId(AppWindow.Id, DisplayAreaFallback.Primary).WorkArea;
        int ww = Math.Min((int)(1680 * k), wa.Width), wh = Math.Min((int)(1020 * k), wa.Height);
        AppWindow.MoveAndResize(new RectInt32(wa.X + (wa.Width - ww) / 2, wa.Y + (wa.Height - wh) / 2, ww, wh));
    }
}

/// Drag strip between the chart and the account pane; the height is remembered.
sealed class SplitHandle : Grid
{
    public SplitHandle(Func<double> get, Action<double> set, Action done)
    {
        Height = 10;
        Background = T1.Clear;
        Children.Add(new Border
        {
            Width = 36, Height = 4, CornerRadius = new CornerRadius(2), Background = new SolidColorBrush(Color.FromArgb(0x60, 0x80, 0x80, 0x80)),
            HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center,
        });
        ProtectedCursor = InputSystemCursor.Create(InputSystemCursorShape.SizeNorthSouth);
        ToolTipService.SetToolTip(this, L("Drag to resize"));
        double? start = null;
        double y0 = 0;
        PointerPressed += (_, e) => { start = get(); y0 = e.GetCurrentPoint(null).Position.Y; CapturePointer(e.Pointer); e.Handled = true; };
        PointerMoved += (_, e) => { if (start is double s0) set(Math.Clamp(s0 - (e.GetCurrentPoint(null).Position.Y - y0), 120, 700)); };
        PointerReleased += (_, e) => { if (start == null) return; start = null; ReleasePointerCapture(e.Pointer); done(); };
    }
}
