import AppKit
import SwiftUI

@main
struct TerminalOneApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) var delegate

    var body: some Scene {
        WindowGroup {
            RootView()
                .frame(minWidth: 1100, minHeight: 700)
                .preferredColorScheme(.dark)
        }
        .windowToolbarStyle(.unified)
        .defaultSize(width: 1680, height: 1020)
        .defaultWindowPlacement { _, _ in
            // test runs open off every screen from the first frame (see AppDelegate)
            ProcessInfo.processInfo.environment["T1_OFFSCREEN"] != nil
                ? WindowPlacement(CGPoint(x: -6000, y: -6000), size: CGSize(width: 1680, height: 1020)) : WindowPlacement()
        }
        .commands { AppCommands() }
    }
}

/// Standard menu items: Settings (Cmd-,) and View > Show/Hide Order Panel (Cmd-Opt-I).
struct AppCommands: Commands {
    @AppStorage("t1.orderPanel") private var orderPanel = true

    var body: some Commands {
        CommandGroup(replacing: .appSettings) {
            Button(L("Settings…")) { NotificationCenter.default.post(name: .init("T1OpenSettings"), object: nil) }
                .keyboardShortcut(",")
        }
        CommandGroup(after: .sidebar) {
            Button(orderPanel ? L("Hide Order Panel") : L("Show Order Panel")) { orderPanel.toggle() }
                .keyboardShortcut("i", modifiers: [.command, .option])
        }
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        // launched as a bare executable (development) it still behaves like an app
        // screenshot runs never take focus or join the user's Stage Manager set
        let env = ProcessInfo.processInfo.environment
        if env["T1_UI_SHOT"] != nil || env["T1_SHOT"] != nil || env["T1_OFFSCREEN"] != nil {
            NSApp.setActivationPolicy(.prohibited)
            // T1_OFFSCREEN=1: development captures; the window lives outside every screen
            if env["T1_OFFSCREEN"] != nil {
                for d in [0.0, 0.3, 1.0, 3.0] {
                    DispatchQueue.main.asyncAfter(deadline: .now() + d) {
                        for w in NSApp.windows where w.frame.width > 400 {
                            // AppKit keeps the title bar on a screen, so also sink the window below the
                            // wallpaper and out of Stage Manager / window cycling
                            w.level = NSWindow.Level(rawValue: Int(CGWindowLevelForKey(.desktopWindow)) - 1)
                            w.collectionBehavior = [.stationary, .ignoresCycle, .transient, .fullScreenNone]
                            // T1_TEST_FRAME="x,y,w,h" (AppKit coords): reproduce a given screen / scale, still below the wallpaper
                            let f = (env["T1_TEST_FRAME"] ?? "").split(separator: ",").compactMap { Double($0) }
                            w.setFrame(f.count == 4 ? NSRect(x: f[0], y: f[1], width: f[2], height: f[3]) : NSRect(x: -6000, y: -6000, width: 1680, height: 1020), display: true)
                        }
                    }
                }
            }
        } else {
            NSApp.setActivationPolicy(.regular)
            NSApp.activate(ignoringOtherApps: true)
        }
        NSApp.appearance = NSAppearance(named: .darkAqua)
        // T1_UI_SHOT=<png> [T1_UI_SHOT_AFTER=<s>]: render the window's views (SwiftUI parts; the
        // Metal chart may come out blank) into a PNG and quit. No screen-recording permission.
        if let path = ProcessInfo.processInfo.environment["T1_UI_SHOT"] {
            let after = Double(ProcessInfo.processInfo.environment["T1_UI_SHOT_AFTER"] ?? "") ?? 15
            DispatchQueue.main.asyncAfter(deadline: .now() + after) {
                if let v = NSApp.windows.first?.contentView?.superview, let rep = v.bitmapImageRepForCachingDisplay(in: v.bounds) {
                    v.cacheDisplay(in: v.bounds, to: rep)
                    try? rep.representation(using: .png, properties: [:])?.write(to: URL(fileURLWithPath: path))
                }
                exit(0)
            }
        }
    }
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }
}

/// Native shell around the egui chart/book: toolbar (pair, mode, settings), stats strip,
/// chart over account tables (resizable), order entry as a trailing inspector (Perp), status bar.
struct RootView: View {
    @State private var store = Store.shared
    @AppStorage("t1.orderPanel") private var orderPanel = true
    @AppStorage("t1.accountHeight") private var accountH = 250.0

    var body: some View {
        let perp = store.state.mode == "Perp"
        VStack(spacing: 0) {
            TopStats()
            // chart + book and the order panel on top; the account strip spans the full width below
            HStack(spacing: 8) {
                VStack(spacing: 0) {
                    ChartBar()
                    // egui draws the chart and the book rows (a fixed 300 pt column on the right);
                    // the book's native header and pages sit on top of that column
                    EguiView().padding([.horizontal, .bottom], 6)
                        .overlay(alignment: .topTrailing) {
                            if store.state.mode != "Option" { BookPanel().frame(width: 300).padding(.trailing, 6) }
                        }
                }
                .frame(minHeight: 280)
                .contentCard()
                if perp && orderPanel {
                    OrderPanel().frame(width: 300)
                        .transition(.move(edge: .trailing).combined(with: .opacity))
                }
            }
            .padding(.horizontal, 8)
            SplitHandle(height: $accountH)
            // ponytail: clip guard; a wide account table must not push the whole window off-screen
            AccountPanel().frame(minWidth: 0, maxWidth: .infinity, alignment: .leading).clipped()
                .contentCard()
                .frame(height: accountH)
                .padding(.horizontal, 8)
            .padding(.horizontal, 8)
            StatusBar()
        }
        .animation(.smooth(duration: 0.25), value: perp && orderPanel)
        .toolbar {
            TopToolbar(store: store)
            ToolbarItem(placement: .primaryAction) {
                Button { orderPanel.toggle() } label: { Label(L("Order Panel"), systemImage: "sidebar.trailing") }
                    .help(L("Show or hide the order panel (Option-Command-I)"))
                    .disabled(!perp)
            }
        }
        .toolbar(removing: .title)
        .environment(store)
    }
}

/// Drag strip between the chart and the account pane; the height is remembered.
struct SplitHandle: View {
    @Binding var height: Double
    @State private var start: Double?
    var body: some View {
        Capsule().fill(.tertiary).frame(width: 36, height: 4)
            .frame(maxWidth: .infinity).frame(height: 10)
            .contentShape(.rect)
            .pointerStyle(.rowResize)
            .gesture(DragGesture(minimumDistance: 1)
                .onChanged { g in
                    let s0 = start ?? height
                    start = s0
                    height = min(max(s0 - g.translation.height, 120), 700)
                }
                .onEnded { _ in start = nil })
            .help(L("Drag to resize"))
    }
}
