import Darwin
import Foundation

let executableURL = URL(fileURLWithPath: CommandLine.arguments[0]).standardizedFileURL
let validationDirectory = executableURL
    .deletingLastPathComponent() // MacOS
    .deletingLastPathComponent() // Contents
    .deletingLastPathComponent() // OpenEventValidation.app
    .deletingLastPathComponent() // Isolated validation directory
let logURL = validationDirectory.appendingPathComponent(
    "viewr-launcher-probe.log",
    isDirectory: false
)
let argument = CommandLine.arguments.dropFirst().first ?? "<missing>"
let entry = "\(getpid())\t\(argument)\n"
let data = Data(entry.utf8)

if !FileManager.default.fileExists(atPath: logURL.path) {
    FileManager.default.createFile(atPath: logURL.path, contents: nil)
}
let handle = try FileHandle(forWritingTo: logURL)
try handle.seekToEnd()
try handle.write(contentsOf: data)
try handle.close()

// Keep each fake viewer alive long enough to prove that the launcher remains
// available for a second Launch Services open-document event.
Thread.sleep(forTimeInterval: 15)
