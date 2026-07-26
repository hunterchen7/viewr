import AppKit
import Foundation

final class ViewrAppDelegate: NSObject, NSApplicationDelegate {
    private var children: [ObjectIdentifier: Process] = [:]
    private var showingFolderPicker = false

    func applicationWillFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
    }

    func applicationOpenUntitledFile(_ sender: NSApplication) -> Bool {
        chooseFolder()
        return true
    }

    func application(_ sender: NSApplication, openFiles filenames: [String]) {
        var launchedEveryFile = !filenames.isEmpty
        for filename in filenames {
            launchedEveryFile = launchViewer(path: filename) && launchedEveryFile
        }

        sender.reply(toOpenOrPrint: launchedEveryFile ? .success : .failure)
        terminateIfIdle()
    }

    private func chooseFolder() {
        guard !showingFolderPicker else {
            return
        }

        showingFolderPicker = true
        NSApp.activate(ignoringOtherApps: true)

        let panel = NSOpenPanel()
        panel.title = "Choose a folder to open in Viewr"
        panel.prompt = "Open"
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = false

        if panel.runModal() == .OK, let folder = panel.url {
            _ = launchViewer(path: folder.path)
        }

        showingFolderPicker = false
        terminateIfIdle()
    }

    @discardableResult
    private func launchViewer(path: String) -> Bool {
        let executable = Bundle.main.bundleURL
            .appendingPathComponent("Contents", isDirectory: true)
            .appendingPathComponent("MacOS", isDirectory: true)
            .appendingPathComponent("viewr-bin", isDirectory: false)

        let process = Process()
        process.executableURL = executable
        process.arguments = [path]
        process.standardInput = FileHandle.nullDevice
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice

        let identifier = ObjectIdentifier(process)
        children[identifier] = process
        process.terminationHandler = { [weak self] _ in
            DispatchQueue.main.async {
                self?.children.removeValue(forKey: identifier)
                self?.terminateIfIdle()
            }
        }

        do {
            try process.run()
            return true
        } catch {
            children.removeValue(forKey: identifier)
            showLaunchError(error)
            return false
        }
    }

    private func terminateIfIdle() {
        guard children.isEmpty, !showingFolderPicker else {
            return
        }
        NSApp.terminate(nil)
    }

    private func showLaunchError(_ error: Error) {
        NSApp.activate(ignoringOtherApps: true)

        let alert = NSAlert()
        alert.alertStyle = .critical
        alert.messageText = "Viewr could not be opened"
        alert.informativeText = error.localizedDescription
        alert.runModal()
    }
}

let application = NSApplication.shared
let delegate = ViewrAppDelegate()
application.delegate = delegate
application.run()
