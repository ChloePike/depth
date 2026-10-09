using System.Runtime.InteropServices;
using System.Text;

namespace Depth;

/// C ABI of t1_ffi.dll (ffi/t1.h). UI thread only. Coordinates in DIPs, top-left origin.
/// mods: 1 shift, 2 ctrl, 4 alt; never 8 on Windows (Rust maps Ctrl to the shortcut modifier).
static unsafe class Native
{
    const string Dll = "t1_ffi.dll";
    const CallingConvention C = CallingConvention.Cdecl;

    /// native: the SwapChainPanel's IInspectable*; null when no GPU device / surface could be created
    [DllImport(Dll, CallingConvention = C)] public static extern IntPtr t1_view_new(IntPtr native, float w, float h, float scale);
    [DllImport(Dll, CallingConvention = C)] public static extern void t1_view_free(IntPtr v);
    [DllImport(Dll, CallingConvention = C)] public static extern void t1_view_resize(IntPtr v, float w, float h, float scale);
    /// 1 when it drew; idle calls cost one comparison (Rust throttles to egui's repaint delay)
    [DllImport(Dll, CallingConvention = C)] public static extern int t1_view_render(IntPtr v);
    [DllImport(Dll, CallingConvention = C)] public static extern void t1_view_pointer_move(IntPtr v, float x, float y);
    [DllImport(Dll, CallingConvention = C)] public static extern void t1_view_pointer_leave(IntPtr v);
    /// button: 0 left, 1 right, 2 middle
    [DllImport(Dll, CallingConvention = C)] public static extern void t1_view_pointer_button(IntPtr v, float x, float y, int button, int pressed, uint mods);
    [DllImport(Dll, CallingConvention = C)] public static extern void t1_view_scroll(IntPtr v, float dx, float dy, uint mods);
    [DllImport(Dll, CallingConvention = C)] public static extern void t1_view_zoom(IntPtr v, float factor);
    [DllImport(Dll, CallingConvention = C)] static extern void t1_view_key(IntPtr v, byte* name, int pressed, uint mods);
    [DllImport(Dll, CallingConvention = C)] static extern void t1_view_text(IntPtr v, byte* text);
    [DllImport(Dll, CallingConvention = C)] static extern void t1_view_paste(IntPtr v, byte* text);
    [DllImport(Dll, CallingConvention = C)] public static extern void t1_view_focus(IntPtr v, int focused);
    [DllImport(Dll, CallingConvention = C)] public static extern void t1_view_appearance(IntPtr v, int dark);
    /// 0 arrow, 1 hand, 2 text, 3 resize h, 4 resize v, 5 crosshair, 6 grab, 7 grabbing, 8 not allowed, 9 NW-SE, 10 NE-SW
    [DllImport(Dll, CallingConvention = C)] public static extern int t1_view_cursor(IntPtr v);
    [DllImport(Dll, CallingConvention = C)] static extern IntPtr t1_view_take_copied(IntPtr v);
    [DllImport(Dll, CallingConvention = C)] static extern IntPtr t1_state(IntPtr v);
    [DllImport(Dll, CallingConvention = C)] static extern IntPtr t1_call(IntPtr v, byte* req);
    [DllImport(Dll, CallingConvention = C)] static extern void t1_free(IntPtr s);

    static byte[] Z(string s) => Encoding.UTF8.GetBytes(s + "\0");

    /// Rust-owned char* to a string, freed.
    static string? Take(IntPtr p)
    {
        if (p == IntPtr.Zero) return null;
        try { return Marshal.PtrToStringUTF8(p); } finally { t1_free(p); }
    }

    /// egui key name ("ArrowLeft", "Enter", "A", ...); unknown names are ignored by Rust.
    public static void Key(IntPtr v, string name, bool pressed, uint mods) { fixed (byte* p = Z(name)) t1_view_key(v, p, pressed ? 1 : 0, mods); }
    public static void Text(IntPtr v, string s) { fixed (byte* p = Z(s)) t1_view_text(v, p); }
    public static void Paste(IntPtr v, string s) { fixed (byte* p = Z(s)) t1_view_paste(v, p); }
    /// Text egui copied since the last call, or null.
    public static string? TakeCopied(IntPtr v) => Take(t1_view_take_copied(v));
    /// Native panels' state snapshot (JSON, ui/src/native.rs state_json).
    public static string? State(IntPtr v) => Take(t1_state(v));
    /// One action ({"op": ...}); returns {"ok": bool, ...}.
    public static string? Call(IntPtr v, string req) { fixed (byte* p = Z(req)) return Take(t1_call(v, p)); }
}
