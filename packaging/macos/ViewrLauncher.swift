import AppKit
import Darwin
import Foundation

private let launcherSelfTestArgument = "--viewr-launcher-self-test"

private func writeSelfTestMessage(_ message: String, to handle: FileHandle) {
    handle.write(Data((message + "\n").utf8))
}

private func runLauncherSelfTest() -> Int32 {
    guard let launcher = Bundle.main.executableURL else {
        writeSelfTestMessage(
            "viewr-launcher self-test failed",
            to: .standardError
        )
        return 1
    }

    let viewer = launcher.deletingLastPathComponent()
        .appendingPathComponent("viewr-bin", isDirectory: false)
    guard FileManager.default.isExecutableFile(atPath: viewer.path) else {
        writeSelfTestMessage(
            "viewr-launcher self-test failed",
            to: .standardError
        )
        return 1
    }

    let output = Pipe()
    let process = Process()
    process.executableURL = viewer
    process.arguments = ["--version"]
    process.standardInput = FileHandle.nullDevice
    process.standardOutput = output
    process.standardError = FileHandle.nullDevice

    do {
        try process.run()
        process.waitUntilExit()
    } catch {
        writeSelfTestMessage(
            "viewr-launcher self-test failed",
            to: .standardError
        )
        return 1
    }

    guard process.terminationReason == .exit,
          process.terminationStatus == 0,
          let reported = String(
              data: output.fileHandleForReading.readDataToEndOfFile(),
              encoding: .utf8
          )
    else {
        writeSelfTestMessage(
            "viewr-launcher self-test failed",
            to: .standardError
        )
        return 1
    }

    let versionOutput = reported.hasSuffix("\n")
        ? String(reported.dropLast())
        : reported
    let prefix = "viewr "
    guard versionOutput.hasPrefix(prefix),
          !versionOutput.dropFirst(prefix.count).isEmpty,
          !versionOutput.contains("\n"),
          !versionOutput.contains("\r")
    else {
        writeSelfTestMessage(
            "viewr-launcher self-test failed",
            to: .standardError
        )
        return 1
    }

    let version = String(versionOutput.dropFirst(prefix.count))
    writeSelfTestMessage("viewr-launcher \(version)", to: .standardOutput)
    return 0
}

if CommandLine.arguments.count == 2,
   CommandLine.arguments[1] == launcherSelfTestArgument
{
    exit(runLauncherSelfTest())
}

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
        let representatives = representativesByFolder(filenames)
        var launchedEveryFolder = !representatives.isEmpty
        for representative in representatives {
            launchedEveryFolder =
                launchViewer(path: representative) && launchedEveryFolder
        }

        sender.reply(toOpenOrPrint: launchedEveryFolder ? .success : .failure)
        terminateIfIdle()
    }

    private func representativesByFolder(
        _ filenames: [String]
    ) -> [String] {
        var seenFolders = Set<String>()
        var representatives: [String] = []

        for filename in filenames {
            let fileURL = URL(fileURLWithPath: filename)
                .resolvingSymlinksInPath()
                .standardizedFileURL
            var isDirectory = ObjCBool(false)
            let exists = FileManager.default.fileExists(
                atPath: fileURL.path,
                isDirectory: &isDirectory
            )
            let folder = exists && isDirectory.boolValue
                ? fileURL.path
                : fileURL.deletingLastPathComponent().path
            if seenFolders.insert(folder).inserted {
                representatives.append(fileURL.path)
            }
        }

        return representatives
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
            let path = folder.resolvingSymlinksInPath().standardizedFileURL.path
            _ = launchViewer(path: path)
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
