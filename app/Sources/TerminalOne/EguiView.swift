import AppKit
import CT1
import QuartzCore
import SwiftUI

/// The Rust/egui terminal rendered into a Metal layer (wgpu), driven by the display link.
struct EguiView: NSViewRepresentable {
    func makeNSView(context: Context) -> EguiNSView { EguiNSView(frame: .zero) }
    func updateNSView(_ view: EguiNSView, context: Context) {}
}

final class EguiNSView: NSView {
    // touched only on the main thread (deinit included: the view is released there)
    nonisolated(unsafe) private var handle: OpaquePointer?
    nonisolated(unsafe) private var link: CADisplayLink?
    private var inside = false
    private var lastCursor = -1
    private var tracking: NSTrackingArea?

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
    }
    required init?(coder: NSCoder) { fatalError() }

    // wgpu renders into this CAMetalLayer
    override func makeBackingLayer() -> CALayer { let l = CAMetalLayer(); l.isOpaque = false; return l }
    override var isFlipped: Bool { true }  // top-left origin, like egui
    override var acceptsFirstResponder: Bool { true }
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    private var scale: Float { Float(window?.backingScaleFactor ?? 2) }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        guard let window else { return }
        window.acceptsMouseMovedEvents = true
        window.titlebarAppearsTransparent = true
        window.isMovableByWindowBackground = false
        if handle == nil {
            handle = t1_view_new(Unmanaged.passUnretained(self).toOpaque(), Float(bounds.width), Float(bounds.height), scale)
            if let h = handle { Store.shared.attach(h) }
            viewDidChangeEffectiveAppearance()
            let l = displayLink(target: self, selector: #selector(tick))
            l.add(to: .main, forMode: .common)
            link = l
        }
        window.makeFirstResponder(self)
    }

    override func layout() {
        super.layout()
        (layer as? CAMetalLayer)?.contentsScale = CGFloat(scale)
        if let h = handle { t1_view_resize(h, Float(bounds.width), Float(bounds.height), scale) }
    }

    /// egui's "system" theme follows this view's appearance (NSApp.appearance, set from the theme pref).
    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        guard let h = handle else { return }
        t1_view_appearance(h, effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? 1 : 0)
    }

    override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        needsLayout = true
    }

    @objc private func tick(_ l: CADisplayLink) {
        guard let h = handle else { return }
        t1_view_render(h)
        let cursors: [NSCursor] = [.arrow, .pointingHand, .iBeam, .resizeLeftRight, .resizeUpDown, .crosshair, .openHand, .closedHand, .operationNotAllowed, .arrow, .arrow]
        // only while the pointer is over the chart, and only on change: native panels own their cursors
        let c = Int(t1_view_cursor(h))
        if inside, c != lastCursor { lastCursor = c; cursors[min(max(c, 0), cursors.count - 1)].set() }
        if let s = t1_view_take_copied(h) {
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(String(cString: s), forType: .string)
            t1_free(s)
        }
    }

    deinit {
        link?.invalidate()
        if let h = handle { t1_view_free(h) }
    }

    // MARK: input

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let t = tracking { removeTrackingArea(t) }
        let t = NSTrackingArea(rect: bounds, options: [.mouseMoved, .mouseEnteredAndExited, .activeAlways, .inVisibleRect], owner: self)
        addTrackingArea(t)
        tracking = t
    }

    private func pt(_ e: NSEvent) -> NSPoint { convert(e.locationInWindow, from: nil) }

    private func mods(_ e: NSEvent) -> UInt32 {
        var m: UInt32 = 0
        if e.modifierFlags.contains(.shift) { m |= 1 }
        if e.modifierFlags.contains(.control) { m |= 2 }
        if e.modifierFlags.contains(.option) { m |= 4 }
        if e.modifierFlags.contains(.command) { m |= 8 }
        return m
    }

    private func move(_ e: NSEvent) { guard let h = handle else { return }; let p = pt(e); t1_view_pointer_move(h, Float(p.x), Float(p.y)) }
    private func button(_ e: NSEvent, _ b: Int32, _ down: Bool) {
        guard let h = handle else { return }
        let p = pt(e)
        t1_view_pointer_button(h, Float(p.x), Float(p.y), b, down ? 1 : 0, mods(e))
    }

    override func mouseMoved(with e: NSEvent) { move(e) }
    override func mouseDragged(with e: NSEvent) { move(e) }
    override func rightMouseDragged(with e: NSEvent) { move(e) }
    override func otherMouseDragged(with e: NSEvent) { move(e) }
    override func mouseEntered(with e: NSEvent) { inside = true; lastCursor = -1 }
    override func mouseExited(with e: NSEvent) { inside = false; NSCursor.arrow.set(); if let h = handle { t1_view_pointer_leave(h) } }
    override func mouseDown(with e: NSEvent) { window?.makeFirstResponder(self); button(e, 0, true) }
    override func mouseUp(with e: NSEvent) { button(e, 0, false) }
    override func rightMouseDown(with e: NSEvent) { button(e, 1, true) }
    override func rightMouseUp(with e: NSEvent) { button(e, 1, false) }
    override func otherMouseDown(with e: NSEvent) { button(e, 2, true) }
    override func otherMouseUp(with e: NSEvent) { button(e, 2, false) }

    override func scrollWheel(with e: NSEvent) {
        guard let h = handle else { return }
        // trackpads report points; wheels report lines (~ 20 pt each here)
        let k: CGFloat = e.hasPreciseScrollingDeltas ? 1 : 20
        t1_view_scroll(h, Float(e.scrollingDeltaX * k), Float(e.scrollingDeltaY * k), mods(e))
    }

    override func magnify(with e: NSEvent) { if let h = handle { t1_view_zoom(h, Float(1 + e.magnification)) } }

    private static let named: [UInt16: String] = [
        123: "ArrowLeft", 124: "ArrowRight", 125: "ArrowDown", 126: "ArrowUp", 36: "Enter", 76: "Enter", 48: "Tab",
        51: "Backspace", 117: "Delete", 53: "Escape", 115: "Home", 119: "End", 116: "PageUp", 121: "PageDown", 49: "Space",
    ]

    private func keyName(_ e: NSEvent) -> String? {
        if let n = Self.named[e.keyCode] { return n }
        guard let c = e.charactersIgnoringModifiers?.uppercased(), c.count == 1, let ch = c.first, ch.isLetter || ch.isNumber else { return nil }
        return String(ch)
    }

    private func key(_ e: NSEvent, _ down: Bool) {
        guard let h = handle, let name = keyName(e) else { return }
        name.withCString { t1_view_key(h, $0, down ? 1 : 0, mods(e)) }
    }

    override func keyDown(with e: NSEvent) {
        guard let h = handle else { return }
        key(e, true)
        // printable text (no IME yet); command / control chords are shortcuts, not text
        if !e.modifierFlags.contains(.command) && !e.modifierFlags.contains(.control),
           let s = e.characters, let u = s.unicodeScalars.first, u.value >= 0x20, u.value != 0x7f, !(0xF700...0xF8FF).contains(u.value) {
            s.withCString { t1_view_text(h, $0) }
        }
    }

    override func keyUp(with e: NSEvent) { key(e, false) }

    override func performKeyEquivalent(with e: NSEvent) -> Bool {
        guard let h = handle, e.modifierFlags.contains(.command), window?.firstResponder === self else { return super.performKeyEquivalent(with: e) }
        if e.charactersIgnoringModifiers == "v", let s = NSPasteboard.general.string(forType: .string) {
            s.withCString { t1_view_paste(h, $0) }
            return true
        }
        if ["c", "x", "a"].contains(e.charactersIgnoringModifiers ?? "") { key(e, true); key(e, false); return true }
        return super.performKeyEquivalent(with: e)
    }
}
