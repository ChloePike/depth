using System.Runtime.InteropServices;
using Microsoft.UI.Input;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Windows.ApplicationModel.DataTransfer;
using Windows.Foundation;
using VirtualKey = Windows.System.VirtualKey;
using VirtualKeyModifiers = Windows.System.VirtualKeyModifiers;

namespace Depth;

/// The Rust/egui terminal (chart, book rows, option chain) rendered by wgpu (DX12) into a
/// SwapChainPanel, driven by CompositionTarget.Rendering. Port of EguiView.swift.
/// A Control (UserControl) so it can take keyboard focus and set its cursor.
sealed class EguiPanel : UserControl
{
    readonly Grid root = new() { Background = T1.Clear };
    readonly Canvas canvas = new();
    readonly SwapChainPanel panel = new();
    readonly ScaleTransform unscale = new();
    IntPtr view, unk;
    double scale = 1;
    int lastCursor = -1;
    bool pressed;
    char high; // pending UTF-16 high surrogate from CharacterReceived

    public EguiPanel()
    {
        IsTabStop = true;
        UseSystemFocusVisuals = false;
        // ponytail: wgpu never calls IDXGISwapChain2::SetMatrixTransform, so the swap chain (sized in
        // pixels) would show at 1 px = 1 DIP. The panel is laid out scale x larger in a Canvas (no layout
        // clip) and shrunk back by a 1/scale render transform; the container clips. Upgrade path: have
        // wgpu set the inverse composition scale on the swap chain, then drop the transform.
        panel.RenderTransform = unscale;
        canvas.Children.Add(panel);
        root.Children.Add(canvas);
        Content = root;

        Loaded += (_, _) =>
        {
            scale = XamlRoot.RasterizationScale;
            XamlRoot.Changed += (_, _) => { if (XamlRoot != null && XamlRoot.RasterizationScale != scale) { scale = XamlRoot.RasterizationScale; Layout(); } };
            Create();
        };
        SizeChanged += (_, _) => { Layout(); Create(); };
        ActualThemeChanged += (_, _) => { if (view != IntPtr.Zero) Native.t1_view_appearance(view, ActualTheme == ElementTheme.Dark ? 1 : 0); };
        GotFocus += (_, _) => { if (view != IntPtr.Zero) Native.t1_view_focus(view, 1); };
        LostFocus += (_, _) => { if (view != IntPtr.Zero) Native.t1_view_focus(view, 0); };

        PointerMoved += (_, e) => { if (view == IntPtr.Zero) return; var p = e.GetCurrentPoint(this).Position; Native.t1_view_pointer_move(view, (float)p.X, (float)p.Y); };
        PointerExited += (_, _) => { if (view != IntPtr.Zero && !pressed) Native.t1_view_pointer_leave(view); };
        PointerPressed += OnPressed;
        PointerReleased += OnReleased;
        PointerWheelChanged += OnWheel;
        KeyDown += (_, e) => OnKey(e, true);
        KeyUp += (_, e) => OnKey(e, false);
        CharacterReceived += OnChar;
    }

    /// The egui view once created (also attached to Store.Shared).
    public IntPtr View => view;

    void Layout()
    {
        double w = ActualWidth, h = ActualHeight;
        panel.Width = Math.Max(1, w * scale);
        panel.Height = Math.Max(1, h * scale);
        unscale.ScaleX = unscale.ScaleY = 1 / scale;
        root.Clip = new RectangleGeometry { Rect = new Rect(0, 0, w, h) };
        if (view != IntPtr.Zero && w >= 1 && h >= 1) Native.t1_view_resize(view, (float)w, (float)h, (float)scale);
    }

    void Create()
    {
        if (view != IntPtr.Zero || unk != IntPtr.Zero || XamlRoot == null || ActualWidth < 1 || ActualHeight < 1) return;
        Layout();
        try
        {
            // wgpu queries ISwapChainPanelNative on it and calls SetSwapChain; the panel outlives the view
            unk = WinRT.MarshalInspectable<object>.FromManaged(panel);
            view = Native.t1_view_new(unk, (float)ActualWidth, (float)ActualHeight, (float)scale);
        }
        catch (Exception e) when (e is DllNotFoundException or EntryPointNotFoundException or BadImageFormatException)
        {
            Fail($"t1_ffi.dll could not be loaded: {e.Message}");
            return;
        }
        if (view == IntPtr.Zero) { Fail("No DirectX 12 device or surface (t1_view_new returned null)."); return; }
        Store.Shared.Attach(view);
        Native.t1_view_appearance(view, ActualTheme == ElementTheme.Dark ? 1 : 0);
        CompositionTarget.Rendering += Tick;
    }

    void Fail(string msg)
    {
        Log.Write("host-crash.log", msg);
        root.Children.Add(new TextBlock { Text = msg, Margin = new Thickness(16), TextWrapping = TextWrapping.Wrap });
    }

    void Tick(object? sender, object e)
    {
        if (view == IntPtr.Zero) return;
        Native.t1_view_render(view);
        int c = Native.t1_view_cursor(view);
        if (c != lastCursor)
        {
            lastCursor = c;
            ProtectedCursor = InputSystemCursor.Create(c switch
            {
                1 => InputSystemCursorShape.Hand,
                2 => InputSystemCursorShape.IBeam,
                3 => InputSystemCursorShape.SizeWestEast,
                4 => InputSystemCursorShape.SizeNorthSouth,
                5 => InputSystemCursorShape.Cross,
                6 => InputSystemCursorShape.Hand,
                7 => InputSystemCursorShape.SizeAll,
                8 => InputSystemCursorShape.UniversalNo,
                9 => InputSystemCursorShape.SizeNorthwestSoutheast,
                10 => InputSystemCursorShape.SizeNortheastSouthwest,
                _ => InputSystemCursorShape.Arrow,
            });
        }
        if (Native.TakeCopied(view) is string s)
        {
            var dp = new DataPackage();
            dp.SetText(s);
            Clipboard.SetContent(dp);
        }
    }

    /// Free the egui view (window closing). The panel stays alive until after t1_view_free.
    public void Dispose()
    {
        CompositionTarget.Rendering -= Tick;
        Store.Shared.Detach();
        if (view != IntPtr.Zero) { Native.t1_view_free(view); view = IntPtr.Zero; }
        if (unk != IntPtr.Zero) { Marshal.Release(unk); unk = IntPtr.Zero; }
    }

    static uint Mods(VirtualKeyModifiers m) =>
        (m.HasFlag(VirtualKeyModifiers.Shift) ? 1u : 0) | (m.HasFlag(VirtualKeyModifiers.Control) ? 2u : 0) | (m.HasFlag(VirtualKeyModifiers.Menu) ? 4u : 0);

    void OnPressed(object sender, PointerRoutedEventArgs e)
    {
        if (view == IntPtr.Zero) return;
        Focus(FocusState.Pointer);
        CapturePointer(e.Pointer);
        pressed = true;
        var pt = e.GetCurrentPoint(this);
        int b = pt.Properties.PointerUpdateKind switch
        {
            PointerUpdateKind.RightButtonPressed => 1,
            PointerUpdateKind.MiddleButtonPressed => 2,
            _ => 0,
        };
        Native.t1_view_pointer_button(view, (float)pt.Position.X, (float)pt.Position.Y, b, 1, Mods(e.KeyModifiers));
        e.Handled = true;
    }

    void OnReleased(object sender, PointerRoutedEventArgs e)
    {
        if (view == IntPtr.Zero) return;
        var pt = e.GetCurrentPoint(this);
        int b = pt.Properties.PointerUpdateKind switch
        {
            PointerUpdateKind.RightButtonReleased => 1,
            PointerUpdateKind.MiddleButtonReleased => 2,
            _ => 0,
        };
        Native.t1_view_pointer_button(view, (float)pt.Position.X, (float)pt.Position.Y, b, 0, Mods(e.KeyModifiers));
        var pp = pt.Properties;
        if (!pp.IsLeftButtonPressed && !pp.IsRightButtonPressed && !pp.IsMiddleButtonPressed)
        {
            pressed = false;
            ReleasePointerCapture(e.Pointer);
            var p = pt.Position;
            if (p.X < 0 || p.Y < 0 || p.X > ActualWidth || p.Y > ActualHeight) Native.t1_view_pointer_leave(view);
        }
        e.Handled = true;
    }

    void OnWheel(object sender, PointerRoutedEventArgs e)
    {
        if (view == IntPtr.Zero) return;
        var pp = e.GetCurrentPoint(this).Properties;
        // wheel notches (120) to points; precision touchpads send fractions of a notch
        float d = pp.MouseWheelDelta / 120f * 50f;
        if (pp.IsHorizontalMouseWheel) Native.t1_view_scroll(view, -d, 0, Mods(e.KeyModifiers));
        else Native.t1_view_scroll(view, 0, d, Mods(e.KeyModifiers));
        e.Handled = true;
    }

    void OnKey(KeyRoutedEventArgs e, bool down)
    {
        if (view == IntPtr.Zero) return;
        uint m = Ui.Mods();
        if (down && e.Key == VirtualKey.V && (m & 2) != 0 && (m & 4) == 0) { Paste(); e.Handled = true; return; }
        if (KeyName(e.Key) is not string name) return;
        Native.Key(view, name, down, m);
        // handled: Tab and arrows stay in egui instead of moving XAML focus
        e.Handled = true;
    }

    async void Paste()
    {
        try
        {
            var c = Clipboard.GetContent();
            if (!c.Contains(StandardDataFormats.Text)) return;
            var s = await c.GetTextAsync();
            if (view != IntPtr.Zero && s != null) Native.Paste(view, s);
        }
        catch (Exception) { } // clipboard busy / denied
    }

    void OnChar(UIElement sender, CharacterReceivedRoutedEventArgs e)
    {
        if (view == IntPtr.Zero) return;
        char c = e.Character;
        // Ctrl chords are shortcuts, not text; AltGr arrives as Ctrl+Alt and types characters
        uint m = Ui.Mods();
        if ((m & 2) != 0 && (m & 4) == 0) return;
        if (char.IsHighSurrogate(c)) { high = c; return; }
        string s = char.IsLowSurrogate(c) && high != '\0' ? new string(new[] { high, c }) : c.ToString();
        high = '\0';
        if (c < 0x20 || c == 0x7f) return;
        Native.Text(view, s);
        e.Handled = true;
    }

    /// egui Key::from_name names.
    static string? KeyName(VirtualKey k)
    {
        int c = (int)k;
        if (c is (>= 65 and <= 90) or (>= 48 and <= 57)) return ((char)c).ToString(); // A-Z, 0-9
        if (k >= VirtualKey.NumberPad0 && k <= VirtualKey.NumberPad9) return ((char)('0' + (k - VirtualKey.NumberPad0))).ToString();
        if (k >= VirtualKey.F1 && k <= VirtualKey.F12) return "F" + (k - VirtualKey.F1 + 1);
        return k switch
        {
            VirtualKey.Up => "ArrowUp",
            VirtualKey.Down => "ArrowDown",
            VirtualKey.Left => "ArrowLeft",
            VirtualKey.Right => "ArrowRight",
            VirtualKey.Enter => "Enter",
            VirtualKey.Escape => "Escape",
            VirtualKey.Tab => "Tab",
            VirtualKey.Back => "Backspace",
            VirtualKey.Delete => "Delete",
            VirtualKey.Home => "Home",
            VirtualKey.End => "End",
            VirtualKey.PageUp => "PageUp",
            VirtualKey.PageDown => "PageDown",
            VirtualKey.Space => "Space",
            VirtualKey.Subtract or (VirtualKey)189 => "Minus", // 189 VK_OEM_MINUS
            VirtualKey.Add => "Plus",
            (VirtualKey)187 => "Equals", // VK_OEM_PLUS is the =/+ key
            _ => null,
        };
    }
}
