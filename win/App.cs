using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Markup;
using Microsoft.UI.Xaml.XamlTypeInfo;

namespace Depth;

/// Code-only WinUI 3 app (no .xaml files). Without a generated XamlTypeInfo the framework resolves
/// the built-in controls' types (XamlControlsResources, NumberBox, ...) through this provider.
sealed class App : Application, IXamlMetadataProvider
{
    readonly XamlControlsXamlMetaDataProvider meta = new();
    MainWindow? window;

    [STAThread]
    static void Main()
    {
        WinRT.ComWrappersSupport.InitializeComWrappers();
        Application.Start(_ =>
        {
            var ctx = new DispatcherQueueSynchronizationContext(DispatcherQueue.GetForCurrentThread());
            SynchronizationContext.SetSynchronizationContext(ctx);
            new App();
        });
    }

    public App()
    {
        // launched from Explorer: stderr is gone, so unhandled exceptions go next to Rust's panic.log
        UnhandledException += (_, e) => Log.Write("host-crash.log", e.Exception?.ToString() ?? e.Message);
    }

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        Resources ??= new ResourceDictionary();
        Resources.MergedDictionaries.Add(new XamlControlsResources());
        window = new MainWindow();
        window.Activate();
    }

    public IXamlType GetXamlType(Type type) => meta.GetXamlType(type);
    public IXamlType GetXamlType(string fullName) => meta.GetXamlType(fullName);
    public XmlnsDefinition[] GetXmlnsDefinitions() => meta.GetXmlnsDefinitions();
}

/// %LOCALAPPDATA%\TerminalOne\Cache\Logs (sys::log_dir on the Rust side).
static class Log
{
    public static void Write(string file, string msg)
    {
        try
        {
            var dir = System.IO.Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "TerminalOne", "Cache", "Logs");
            Directory.CreateDirectory(dir);
            File.AppendAllText(System.IO.Path.Combine(dir, file), $"{DateTimeOffset.UtcNow.ToUnixTimeSeconds()} {msg}\n");
        }
        catch (Exception) { }
    }
}
