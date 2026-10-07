import SwiftUI

/// API keys per venue, stored in the macOS Keychain by the engine. Secrets are write-only: the
/// form never shows a stored secret (only the key's last four characters) and clears itself.
struct SettingsKeys: View {
    @Environment(Store.self) private var store
    @State private var editing: String?
    @State private var key = ""
    @State private var secret = ""
    @State private var extra = ""
    @State private var error: String?
    @State private var removing: String?
    @State private var testing: Set<String> = []

    var body: some View {
        Form {
            Section {
                Label {
                    Text(L("Never enable withdrawals on these keys. Trading needs trade permission, transfers need transfer permission. Start with a read-only key."))
                } icon: { Image(systemName: "exclamationmark.shield.fill").foregroundStyle(Color.orange) }
                .font(uiFont(11.5)).foregroundStyle(Color.primary)
            } footer: {
                Text(L("Stored in the macOS Keychain (service \"terminal-one\"), never written to a file."))
                    .font(uiFont(11)).foregroundStyle(Color.t1Dim)
            }
            Section(L("Venues")) {
                ForEach(store.state.keys) { k in
                    row(k)
                    if editing == k.ex { form(k) }
                }
            }
        }
        .confirmationDialog(L("Remove API key?"), isPresented: Binding(get: { removing != nil }, set: { if !$0 { removing = nil } }), titleVisibility: .visible, presenting: removing) { ex in
            Button(L("Cancel"), role: .cancel) {}
            Button(L("Remove"), role: .destructive) {
                if store.call("delete_keys", ["ex": ex])?["ok"] as? Bool != true { error = store.lastError }
            }
        } message: { ex in Text("\(ex): " + L("The key is deleted from the Keychain and the venue disconnects from your account.")) }
    }

    private func row(_ k: KeyStatus) -> some View {
        HStack(spacing: 8) {
            VenueIcon(ex: k.ex, size: 18)
            Text(k.ex).font(uiFont(13, .medium))
            if !k.verified {
                AccountBadge(text: L("Untested"), color: Color.orange)
                    .help(L("Implemented and unit-tested, but not yet run against a live account. Test with a read-only key first, then trade the minimum size."))
            }
            Spacer()
            if k.configured {
                VStack(alignment: .trailing, spacing: 2) {
                    HStack(spacing: 4) {
                        Image(systemName: "checkmark.circle.fill").font(.system(size: 10))
                        Text("\(L("Configured")) ••••\(k.tail ?? "")").font(numFont(11))
                    }
                    .foregroundStyle(Color.t1Up)
                    if let t = k.test {
                        Text(t.msg).font(uiFont(10.5)).foregroundStyle(t.ok ? Color.t1Up : Color.t1Down).lineLimit(2).frame(maxWidth: 260, alignment: .trailing)
                            .help(t.msg)
                    } else if testing.contains(k.ex) {
                        Text(L("Testing…")).font(uiFont(10.5)).foregroundStyle(Color.secondary)
                    }
                }
                Button(L("Test")) {
                    testing.insert(k.ex)
                    if store.call("test_keys", ["ex": k.ex])?["ok"] as? Bool != true { error = store.lastError }
                }
                .help(L("Read-only check: balances and permissions"))
                Button(L("Replace")) { open(k.ex) }
                Button(L("Remove"), role: .destructive) { removing = k.ex }
            } else {
                Text(L("Not configured")).font(uiFont(11)).foregroundStyle(Color.t1Dim)
                Button(L("Add…")) { open(k.ex) }
            }
        }
        .controlSize(.small)
        .padding(.vertical, 2)
    }

    @ViewBuilder private func form(_ k: KeyStatus) -> some View {
        let labels = k.labels + [nil, nil, nil]
        VStack(alignment: .leading, spacing: 8) {
            field(L(labels[0] ?? "API key")) {
                TextField("", text: $key).textFieldStyle(.roundedBorder).font(numFont(12)).autocorrectionDisabled()
            }
            field(L(labels[1] ?? "Secret")) {
                SecureField("", text: $secret).textFieldStyle(.roundedBorder).font(numFont(12))
            }
            if let x = labels[2] {
                field(L(x)) { SecureField("", text: $extra).textFieldStyle(.roundedBorder).font(numFont(12)) }
            }
            Label(L("Never enable withdrawal permission."), systemImage: "exclamationmark.triangle.fill")
                .font(uiFont(11)).foregroundStyle(Color.orange)
            if let error { Text(error).font(uiFont(11)).foregroundStyle(Color.t1Down).fixedSize(horizontal: false, vertical: true) }
            HStack {
                Spacer()
                Button(L("Cancel")) { close() }
                Button(L("Save to Keychain")) { save(k) }
                    .buttonStyle(.borderedProminent)
                    .disabled(key.trimmingCharacters(in: .whitespaces).isEmpty || secret.isEmpty || (needsExtra(k.ex) && extra.isEmpty))
            }
        }
        .padding(12)
        .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 8))
    }

    /// OKX and Bitget keys are unusable without their passphrase.
    private func needsExtra(_ ex: String) -> Bool { ex == "Okx" || ex == "Bitget" }

    private func field<F: View>(_ label: String, @ViewBuilder _ f: () -> F) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(label).font(uiFont(11)).foregroundStyle(Color.secondary)
            f()
        }
    }

    private func open(_ ex: String) { close(); editing = ex }

    private func close() { editing = nil; key = ""; secret = ""; extra = ""; error = nil }

    private func save(_ k: KeyStatus) {
        let args: [String: Any] = ["ex": k.ex, "key": key.trimmingCharacters(in: .whitespacesAndNewlines),
                                   "secret": secret.trimmingCharacters(in: .whitespacesAndNewlines), "extra": extra.trimmingCharacters(in: .whitespacesAndNewlines)]
        if store.call("save_keys", args)?["ok"] as? Bool == true {
            testing.insert(k.ex) // the engine tests a newly saved key right away
            close()
        } else {
            error = store.lastError ?? L("Failed")
            secret = ""; extra = ""
        }
    }
}
