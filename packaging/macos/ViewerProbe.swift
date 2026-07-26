import AppKit
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
let activationPolicy = NSApplication.shared.activationPolicy() == .regular
    ? "regular"
    : "nonregular"
let entry = "\(getpid())\t\(getppid())\t\(activationPolicy)\t\(argument)\n"
let data = Data(entry.utf8)

let descriptor = Darwin.open(
    logURL.path,
    O_WRONLY | O_CREAT | O_APPEND,
    S_IRUSR | S_IWUSR
)
guard descriptor >= 0 else {
    throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
}

let written = data.withUnsafeBytes { bytes in
    Darwin.write(descriptor, bytes.baseAddress, bytes.count)
}
let writeError = errno
let closeResult = Darwin.close(descriptor)
guard written == data.count else {
    throw POSIXError(POSIXErrorCode(rawValue: writeError) ?? .EIO)
}
guard closeResult == 0 else {
    throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
}

// Keep each fake viewer alive long enough to prove that the launcher remains
// available for a second Launch Services open-document event.
Thread.sleep(forTimeInterval: 15)
