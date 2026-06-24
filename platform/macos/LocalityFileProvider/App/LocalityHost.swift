import AppKit

final class LocalityHostDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
    }
}

let app = NSApplication.shared
let delegate = LocalityHostDelegate()
app.delegate = delegate
app.run()
