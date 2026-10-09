using System.Reflection;
using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.UI.Dispatching;

namespace Depth;

static class Json
{
    /// Rust's JSON is snake_case; digits do not split words ("high24"), so "_1h" names carry [JsonPropertyName].
    public static readonly JsonSerializerOptions Opts = new() { PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower };
}

/// The app's view of the Rust engine (port of Store.swift): a state snapshot polled from t1_state at
/// 2 Hz and actions sent through t1_call. UI thread only.
/// Views subscribe to Changed and rebuild only what Dirty(section) reports; a language or prefs
/// change (theme, accent, up/down colors) marks every section dirty.
sealed class Store
{
    public static Store Shared { get; } = new();

    public AppState State { get; private set; } = new();
    /// last action error, shown by the panels
    public string? LastError { get; private set; }
    public event Action? Changed;

    IntPtr handle;
    DispatcherQueueTimer? timer;
    // raw JSON per top-level section, this poll and the previous one
    Dictionary<string, string> raw = new(), prev = new();
    static readonly Dictionary<string, PropertyInfo> props =
        typeof(AppState).GetProperties().ToDictionary(p => Json.Opts.PropertyNamingPolicy!.ConvertName(p.Name));

    public void Attach(IntPtr h)
    {
        handle = h;
        timer?.Stop();
        // 2 Hz: tables and numbers; the chart and book rows draw in egui at display rate
        timer = DispatcherQueue.GetForCurrentThread().CreateTimer();
        timer.Interval = TimeSpan.FromMilliseconds(500);
        timer.Tick += (_, _) => Poll();
        timer.Start();
        Poll();
    }

    public void Detach() { timer?.Stop(); handle = IntPtr.Zero; }

    public void Poll()
    {
        if (handle == IntPtr.Zero || Native.State(handle) is not string js) return;
        JsonDocument doc;
        try { doc = JsonDocument.Parse(js); } catch (JsonException) { return; }
        var st = new AppState();
        var next = new Dictionary<string, string>();
        using (doc)
        {
            foreach (var p in doc.RootElement.EnumerateObject())
            {
                next[p.Name] = p.Value.GetRawText();
                if (!props.TryGetValue(p.Name, out var pi) || p.Value.ValueKind == JsonValueKind.Null) continue;
                // one section that fails to decode keeps its defaults instead of dropping the snapshot
                try { pi.SetValue(st, p.Value.Deserialize(pi.PropertyType, Json.Opts)); } catch (Exception) { }
            }
        }
        prev = raw;
        raw = next;
        State = st;
        if (!Same("lang") || !Same("prefs")) prev = new();
        Changed?.Invoke();
    }

    /// Any of these sections changed since the previous snapshot (or a full refresh is pending).
    public bool Dirty(params string[] sections) => sections.Any(s => !Same(s));
    bool Same(string k) => prev.TryGetValue(k, out var a) && raw.TryGetValue(k, out var b) && a == b;

    /// Raise Changed with every section dirty (theme switch).
    public void Refresh() { prev = new(); Changed?.Invoke(); }

    /// Send one action; returns the reply object (null or {"ok": false, ...} sets LastError).
    /// A successful call polls at once. Never call from a Changed handler.
    public JsonObject? Call(string op, JsonObject? args = null)
    {
        if (handle == IntPtr.Zero) return null;
        var body = args ?? new JsonObject();
        body["op"] = op;
        JsonObject? obj = null;
        try { obj = JsonNode.Parse(Native.Call(handle, body.ToJsonString()) ?? "") as JsonObject; } catch (JsonException) { }
        if (obj?["ok"]?.GetValue<bool>() == true) { LastError = null; Poll(); }
        else LastError = obj?["error"]?.GetValue<string>() ?? "failed";
        return obj;
    }

    /// Typed call for replies with a payload (route preview).
    public T? Call<T>(string op, JsonObject? args = null) where T : class
    {
        var o = Call(op, args);
        if (o?["ok"]?.GetValue<bool>() != true) return null;
        try { return o.Deserialize<T>(Json.Opts); } catch (JsonException) { return null; }
    }

    /// Read-only query polled while a view is open (tickers, venue_share, book_levels):
    /// no state refresh, no error bookkeeping.
    public T? Query<T>(string op, JsonObject? args = null) where T : class
    {
        if (handle == IntPtr.Zero) return null;
        var body = args ?? new JsonObject();
        body["op"] = op;
        try { return JsonSerializer.Deserialize<T>(Native.Call(handle, body.ToJsonString()) ?? "", Json.Opts); } catch (JsonException) { return null; }
    }

    /// Settings write through with the full object; there is no Save.
    public void SetPrefs(Prefs p) => Call("set_prefs", new JsonObject { ["prefs"] = JsonSerializer.SerializeToNode(p, Json.Opts) });
    public void SetRoute(RoutePolicy r) => Call("set_route", new JsonObject { ["route"] = JsonSerializer.SerializeToNode(r, Json.Opts) });
}

/// Host-only UI state (panel sizes, favorites, recent pairs): %APPDATA%\TerminalOne\windows-host.json,
/// next to the Rust settings. The macOS host keeps these in UserDefaults.
static class HostPrefs
{
    static readonly string path = System.IO.Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "TerminalOne", "windows-host.json");
    static JsonObject? data;

    static JsonObject Data
    {
        get
        {
            if (data == null)
            {
                try { data = JsonNode.Parse(File.ReadAllText(path)) as JsonObject; } catch (Exception) { }
                data ??= new JsonObject();
            }
            return data;
        }
    }

    public static T Get<T>(string key, T def)
    {
        try { return Data[key] is JsonNode n ? n.Deserialize<T>() ?? def : def; } catch (Exception) { return def; }
    }

    public static void Set<T>(string key, T value)
    {
        Data[key] = JsonSerializer.SerializeToNode(value);
        try
        {
            Directory.CreateDirectory(System.IO.Path.GetDirectoryName(path)!);
            File.WriteAllText(path, Data.ToJsonString());
        }
        catch (Exception) { }
    }
}
