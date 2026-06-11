import AppKit

final class AgentFSHostDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
    }
}

let app = NSApplication.shared
let delegate = AgentFSHostDelegate()
app.delegate = delegate
app.run()
