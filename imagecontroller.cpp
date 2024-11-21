#include "imagecontroller.h"
#include <filesystem>
#include <stdexcept>
#include <algorithm>
#include <QFileInfo>
#include <QString>
#include <QtConcurrent>
#include <QMutex>
#include <filesystem>

namespace fs = std::filesystem;

ImageController::ImageController() : currentIndex(0), cacheManager(), cancelPreloading(false) {
    connect(&cacheManager, &CacheManager::cacheSizeChanged, this, &ImageController::cacheSizeChanged);
}

ImageController::~ImageController() {}

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
        startPreloading();
    }
}

void ImageController::previousImage() {
    if (!fileList.empty()) {
        currentIndex = (currentIndex - 1 + fileList.size()) % fileList.size();
        startPreloading();
    }
}

bool ImageController::isSupportedFile(const std::string &file) const {
    static const std::vector<std::string> supportedExtensions = {".jpg", ".jpeg", ".png"};
    std::string ext = fs::path(file).extension().string();
    std::transform(ext.begin(), ext.end(), ext.begin(), ::tolower);
    return std::find(supportedExtensions.begin(), supportedExtensions.end(), ext) != supportedExtensions.end();
}

QFileInfo ImageController::getFileInfo() {
    return QFileInfo(QString::fromStdString(this->getCurrentFile()));
}

QPixmap ImageController::getPixMap() {
    // Normalize the file path to ensure consistent cache keys
    std::string currentFile = std::filesystem::absolute(this->getCurrentFile()).string();

    // Check if the image is in the cache
    QPixmap* cachedPixmap = cacheManager.getImageFromCache(currentFile);
    if (cachedPixmap != nullptr) {
        return *cachedPixmap; // Return the cached pixmap
    }

    // If not in cache, load the pixmap
    QPixmap pixmap(QString::fromStdString(currentFile));
    if (!pixmap.isNull()) {
        // Store it in the cache for future use
        cacheManager.storeImageInCache(currentFile, pixmap);
    }

    return pixmap; // Return the newly loaded pixmap (or an empty one if loading failed)
}

void ImageController::startPreloading() {
    // Cancel any currently running tasks
    cancelPreloading.store(true);

    // Allow new tasks to proceed
    cancelPreloading.store(false);

    const int totalFiles = fileList.size();
    const int forwardCacheSize = (2 * cacheManager.getMaxCacheSize()) / 3;
    const int backwardCacheSize = cacheManager.getMaxCacheSize() / 3;

    // Forward caching for even indices
    QThreadPool::globalInstance()->start([this, forwardCacheSize, totalFiles]() {
        for (int offset = 0; offset < forwardCacheSize; offset += 2) {
            if (cancelPreloading.load()) {
                qDebug() << "Forward even caching task cancelled";
                return; // Exit task early
            }

            int preloadIndex = (currentIndex + offset + totalFiles) % totalFiles;
            std::string imageID = std::filesystem::absolute(fileList[preloadIndex]).string();

            QMutexLocker locker(&cacheMutex);

            if (!cacheManager.cacheContainsImage(imageID)) {
                QPixmap pixmap(QString::fromStdString(imageID));
                if (!pixmap.isNull()) {
                    cacheManager.storeImageInCache(imageID, pixmap);
                }
            }
        }
    });

    // Forward caching for odd indices
    QThreadPool::globalInstance()->start([this, forwardCacheSize, totalFiles]() {
        for (int offset = 1; offset < forwardCacheSize; offset += 2) {
            if (cancelPreloading.load()) {
                qDebug() << "Forward odd caching task cancelled";
                return; // Exit task early
            }

            int preloadIndex = (currentIndex + offset + totalFiles) % totalFiles;
            std::string imageID = std::filesystem::absolute(fileList[preloadIndex]).string();

            QMutexLocker locker(&cacheMutex);

            if (!cacheManager.cacheContainsImage(imageID)) {
                QPixmap pixmap(QString::fromStdString(imageID));
                if (!pixmap.isNull()) {
                    cacheManager.storeImageInCache(imageID, pixmap);
                }
            }
        }
    });

    // Backward caching
    QThreadPool::globalInstance()->start([this, backwardCacheSize, totalFiles]() {
        for (int offset = -1; offset >= -backwardCacheSize; --offset) {
            if (cancelPreloading.load()) {
                qDebug() << "Backward caching task cancelled";
                return; // Exit task early
            }

            int preloadIndex = (currentIndex + offset + totalFiles) % totalFiles;
            std::string imageID = std::filesystem::absolute(fileList[preloadIndex]).string();

            QMutexLocker locker(&cacheMutex);

            if (!cacheManager.cacheContainsImage(imageID)) {
                QPixmap pixmap(QString::fromStdString(imageID));
                if (!pixmap.isNull()) {
                    cacheManager.storeImageInCache(imageID, pixmap);
                }
            }
        }
    });
}


int ImageController::getCacheSize() {
    // cacheManager.printCachedImages();
    return cacheManager.getCurrentCacheSize();
}

int ImageController::getMaxCacheSize() {
    return cacheManager.getMaxCacheSize();
}

