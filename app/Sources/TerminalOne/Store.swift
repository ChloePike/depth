import CT1
import Foundation
import Observation

/// The app's view of the Rust engine: a state snapshot polled from `t1_state` and actions
/// sent through `t1_call`. Main thread only.
@MainActor @Observable
final class Store {
    static let shared = Store()

    let state = AppState()
    /// last action error, shown by the panels
    var lastError: String?
    @ObservationIgnored var handle: OpaquePointer?
    @ObservationIgnored private var timer: Timer?

    func attach(_ h: OpaquePointer) {
        handle = h
        timer?.invalidate()
        // 2 Hz: tables and numbers (each update re-lays out SwiftUI); the book polls its own levels at 10 Hz, the chart draws at the display rate
        timer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { _ in MainActor.assumeIsolated { Store.shared.poll() } }
        poll()
    }

    func poll() {
        guard let h = handle, let p = t1_state(h) else { return }
        let data = Data(String(cString: p).utf8)
        t1_free(p)
        if let s = try? JSONDecoder().decode(Snapshot.self, from: data) { state.apply(s) }
    }

    /// Send one action; returns the decoded reply object (or nil, with `lastError` set).
    @discardableResult
    func call(_ op: String, _ args: [String: Any] = [:]) -> [String: Any]? {
        guard let h = handle else { return nil }
        var body = args
        body["op"] = op
        guard let json = try? JSONSerialization.data(withJSONObject: body), let s = String(data: json, encoding: .utf8) else { return nil }
        guard let out = s.withCString({ t1_call(h, $0) }) else { return nil }
        let reply = String(cString: out)
        t1_free(out)
        let obj = (try? JSONSerialization.jsonObject(with: Data(reply.utf8))) as? [String: Any]
        if obj?["ok"] as? Bool == true { lastError = nil; poll() } else { lastError = obj?["error"] as? String ?? "failed" }
        return obj
    }

    /// Read-only query at display rate (book levels): no state refresh, no error bookkeeping.
    func query<T: Decodable>(_ op: String, _ args: [String: Any] = [:], as: T.Type) -> T? {
        guard let h = handle else { return nil }
        var body = args
        body["op"] = op
        guard let json = try? JSONSerialization.data(withJSONObject: body), let s = String(data: json, encoding: .utf8),
              let out = s.withCString({ t1_call(h, $0) }) else { return nil }
        let data = Data(String(cString: out).utf8)
        t1_free(out)
        return try? JSONDecoder().decode(T.self, from: data)
    }

    /// Typed call for replies with a payload (route preview, tickers).
    func call<T: Decodable>(_ op: String, _ args: [String: Any] = [:], as: T.Type) -> T? {
        guard let obj = call(op, args), obj["ok"] as? Bool == true, let data = try? JSONSerialization.data(withJSONObject: obj) else { return nil }
        return try? JSONDecoder().decode(T.self, from: data)
    }
}
