import UserNotifications

/// macOS notifications for fired price alerts (the sound is played by the Rust side).
final class Notifier: NSObject, UNUserNotificationCenterDelegate, @unchecked Sendable {
    static let shared = Notifier()
    private var asked = false

    /// Asked once, the first time an alert is armed.
    func authorize() {
        guard !asked else { return }
        asked = true
        let c = UNUserNotificationCenter.current()
        c.delegate = self
        c.requestAuthorization(options: [.alert]) { _, _ in }
    }

    func post(_ title: String, _ body: String) {
        authorize()
        let n = UNMutableNotificationContent()
        n.title = title
        n.body = body
        UNUserNotificationCenter.current().add(UNNotificationRequest(identifier: UUID().uuidString, content: n, trigger: nil))
    }

    /// shown as a banner even while Depth is in front
    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification) async -> UNNotificationPresentationOptions { [.banner, .list] }
}
