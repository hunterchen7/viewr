import AppKit
import CoreFoundation
import Foundation
import UniformTypeIdentifiers

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data((message + "\n").utf8))
    exit(1)
}

let arguments = CommandLine.arguments
guard arguments.count == 6 else {
    fail(
        "usage: probe present|absent|default|type-default|explicit-binding " +
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

func hasExplicitARWBinding() -> Bool {
    let domain = "com.apple.LaunchServices/com.apple.launchservices.secure"
    guard let value = CFPreferencesCopyAppValue(
        "LSHandlers" as CFString,
        domain as CFString
    ) else {
        return false
    }
    guard let handlers = value as? [[String: Any]] else {
        fail("Launch Services handler preferences have an unexpected representation")
    }

    var applicableTypes = Set([
        arguments[3],
        UTType.rawImage.identifier,
        UTType.image.identifier,
    ])
    for type in [
        UTType(importedAs: arguments[3]),
        UTType(filenameExtension: arwURL.pathExtension),
    ].compactMap({ $0 }) {
        applicableTypes.insert(type.identifier)
        applicableTypes.formUnion(type.supertypes.map(\.identifier))
    }

    return handlers.contains { handler in
        let openerRoleKeys = [
            "LSHandlerRoleAll",
            "LSHandlerRoleViewer",
            "LSHandlerRoleEditor",
        ]
        guard openerRoleKeys.contains(where: { key in
            guard let identifier = handler[key] as? String else {
                return false
            }
            return !identifier.isEmpty && identifier != "-"
        }) else {
            return false
        }

        if let contentType = handler["LSHandlerContentType"] as? String {
            if applicableTypes.contains(where: {
                $0.caseInsensitiveCompare(contentType) == .orderedSame
            }) {
                return true
            }
            if let resolvedType = UTType(contentType),
               resolvedType.tags[.filenameExtension, default: []]
               .contains(where: {
                   $0.caseInsensitiveCompare("arw") == .orderedSame
               }) {
                return true
            }
        }

        guard let contentTag = handler["LSHandlerContentTag"] as? String,
              let tagClass = handler["LSHandlerContentTagClass"] as? String
        else {
            return false
        }
        switch tagClass {
        case UTTagClass.filenameExtension.rawValue:
            return contentTag.caseInsensitiveCompare("arw") == .orderedSame
        case UTTagClass.mimeType.rawValue:
            return contentTag.caseInsensitiveCompare("image/x-sony-arw") == .orderedSame
        default:
            return false
        }
    }
}

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
case "explicit-binding":
    let value = hasExplicitARWBinding() ? "present\n" : "absent\n"
    FileHandle.standardOutput.write(Data(value.utf8))
default:
    fail("unknown probe mode")
}
