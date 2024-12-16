#include "MetadataManager.h"
#include <stdexcept>

MetadataManager::MetadataManager(const std::string& filePath) : filePath(filePath) {
    try {
        image = Exiv2::ImageFactory::open(filePath);
        if (!image) {
            throw std::runtime_error("Failed to open image file.");
        }
    } catch (const std::exception& e) {
        throw std::runtime_error(std::string("Exiv2 error: ") + e.what());
    }
}

MetadataManager::~MetadataManager() {
    // Destructor
}

void MetadataManager::loadMetadata() {
    try {
        image->readMetadata();
    } catch (const std::exception& e) {
        throw std::runtime_error(std::string("Error reading metadata: ") + e.what());
    }
}

std::map<std::string, std::string> MetadataManager::getMetadata() const {
    std::map<std::string, std::string> metadata;
    auto& exifData = image->exifData();

    for (const auto& entry : exifData) {
        metadata[entry.key()] = entry.toString();
    }

    return metadata;
}

void MetadataManager::updateRating(int rating) {
    try {
        auto& exifData = image->exifData();
        exifData["Exif.Image.Rating"] = rating;
    } catch (const std::exception& e) {
        throw std::runtime_error(std::string("Error updating rating: ") + e.what());
    }
}

void MetadataManager::saveMetadata() {
    try {
        image->writeMetadata();
    } catch (const std::exception& e) {
        throw std::runtime_error(std::string("Error saving metadata: ") + e.what());
    }
}
