#include "ImageController.h"
#include <filesystem>
#include <stdexcept>
#include <algorithm>

namespace fs = std::filesystem;

ImageController::ImageController() : currentIndex(0) {}

void ImageController::initialize(const std::string &path) {
    fileList.clear();
    currentIndex = 0;

    if (!fs::exists(path)) {
        throw std::invalid_argument("Path does not exist: " + path);
    }

    if (fs::is_regular_file(path)) {
        // Single file: Add it to the file list if supported
        if (isSupportedFile(path)) {
            fileList.push_back(path);
        } else {
            throw std::invalid_argument("Unsupported file format: " + path);
        }
    } else if (fs::is_directory(path)) {
        // Directory: Scan for supported files
        scanDirectory(path);
        if (fileList.empty()) {
            throw std::runtime_error("No supported image files found in directory: " + path);
        }
    } else {
        throw std::invalid_argument("Invalid path type: " + path);
    }
}

void ImageController::scanDirectory(const std::string &directory) {
    for (const auto &entry : fs::directory_iterator(directory)) {
        if (entry.is_regular_file() && isSupportedFile(entry.path().string())) {
            fileList.push_back(entry.path().string());
        }
    }
    std::sort(fileList.begin(), fileList.end());  // Ensure consistent ordering
}

std::string ImageController::getCurrentFile() const {
    if (fileList.empty()) return "";
    return fileList[currentIndex];
}

void ImageController::nextImage() {
    if (!fileList.empty()) {
        currentIndex = (currentIndex + 1) % fileList.size();
    }
}

void ImageController::previousImage() {
    if (!fileList.empty()) {
        currentIndex = (currentIndex - 1 + fileList.size()) % fileList.size();
    }
}

bool ImageController::isSupportedFile(const std::string &file) const {
    static const std::vector<std::string> supportedExtensions = {".jpg", ".jpeg", ".png"};
    std::string ext = fs::path(file).extension().string();
    std::transform(ext.begin(), ext.end(), ext.begin(), ::tolower);
    return std::find(supportedExtensions.begin(), supportedExtensions.end(), ext) != supportedExtensions.end();
}
