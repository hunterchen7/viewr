import AppKit
import Foundation
import UniformTypeIdentifiers

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data((message + "\n").utf8))
    exit(1)
}

let arguments = CommandLine.arguments
guard arguments.count == 5 else {
    fail("usage: probe present|absent|default BUNDLE-ID UTI APP-PATH")
}

let expectedPath = URL(fileURLWithPath: arguments[4])
    .resolvingSymlinksInPath()
    .standardizedFileURL
    .path
let bundlePaths = NSWorkspace.shared
    .urlsForApplications(withBundleIdentifier: arguments[2])
    .map { $0.resolvingSymlinksInPath().standardizedFileURL.path }
let typePaths = NSWorkspace.shared
    .urlsForApplications(toOpen: UTType(importedAs: arguments[3]))
    .map { $0.resolvingSymlinksInPath().standardizedFileURL.path }
let defaultPath = NSWorkspace.shared
    .urlForApplication(toOpen: UTType(importedAs: arguments[3]))?
    .resolvingSymlinksInPath()
    .standardizedFileURL
    .path

switch arguments[1] {
case "present":
    guard bundlePaths == [expectedPath] else {
        fail("Launch Services bundle lookup did not resolve only the installed app")
    }
    guard typePaths.contains(expectedPath) else {
        fail("Launch Services did not register the installed ARW handler")
    }
case "absent":
    guard bundlePaths.isEmpty, !typePaths.contains(expectedPath) else {
        fail("Launch Services still resolves the removed app")
    }
case "default":
    if let defaultPath {
        FileHandle.standardOutput.write(Data((defaultPath + "\n").utf8))
    }
default:
    fail("unknown probe mode")
}
