import AppKit
import Foundation
import UniformTypeIdentifiers

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data((message + "\n").utf8))
    exit(1)
}

let arguments = CommandLine.arguments
guard arguments.count == 6 else {
    fail(
        "usage: probe present|absent|default|type-default " +
            "BUNDLE-ID UTI APP-PATH ARW-FILE"
    )
}

func standardizedPath(_ url: URL) -> String {
    url.resolvingSymlinksInPath().standardizedFileURL.path
}

let expectedPath = standardizedPath(URL(fileURLWithPath: arguments[4]))
let arwURL = URL(fileURLWithPath: arguments[5])
var isDirectory = ObjCBool(false)
guard FileManager.default.fileExists(
    atPath: arwURL.path,
    isDirectory: &isDirectory
), !isDirectory.boolValue else {
    fail("ARW fixture is not a regular file")
}

let bundlePaths = NSWorkspace.shared
    .urlsForApplications(withBundleIdentifier: arguments[2])
    .map(standardizedPath)
let typePaths = NSWorkspace.shared
    .urlsForApplications(toOpen: UTType(importedAs: arguments[3]))
    .map(standardizedPath)
let filePaths = NSWorkspace.shared
    .urlsForApplications(toOpen: arwURL)
    .map(standardizedPath)
let typeDefaultPath = NSWorkspace.shared
    .urlForApplication(toOpen: UTType(importedAs: arguments[3]))?
    .resolvingSymlinksInPath().standardizedFileURL.path
let fileDefaultPath = NSWorkspace.shared
    .urlForApplication(toOpen: arwURL)?
    .resolvingSymlinksInPath().standardizedFileURL.path

switch arguments[1] {
case "present":
    guard bundlePaths == [expectedPath] else {
        fail("Launch Services bundle lookup did not resolve only the installed app")
    }
    guard typePaths.contains(expectedPath) else {
        fail("Launch Services did not register the installed ARW UTI handler")
    }
    guard filePaths.contains(expectedPath) else {
        fail("Launch Services did not register the installed ARW file handler")
    }
case "absent":
    guard bundlePaths.isEmpty,
          !typePaths.contains(expectedPath),
          !filePaths.contains(expectedPath)
    else {
        fail("Launch Services still resolves the removed app")
    }
case "default":
    if let fileDefaultPath {
        FileHandle.standardOutput.write(Data((fileDefaultPath + "\n").utf8))
    }
case "type-default":
    if let typeDefaultPath {
        FileHandle.standardOutput.write(Data((typeDefaultPath + "\n").utf8))
    }
default:
    fail("unknown probe mode")
}
