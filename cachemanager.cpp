#include "CacheManager.h"
#include <stdexcept>

CacheManager::CacheManager(int size) : cacheSize(size) {
    if (size <= 0) {
        throw std::invalid_argument("Cache size must be greater than zero.");
    }
}

CacheManager::~CacheManager() {}

QPixmap* CacheManager::getImageFromCache(const std::string& imageID) {
    auto it = cacheMap.find(imageID);
    if (it == cacheMap.end()) {
        return nullptr; // Image not found in cache
    }

    // Move the accessed item to the front of the list (most recently used)
    cacheList.splice(cacheList.begin(), cacheList, it->second);
    return &(it->second->second);
}

bool CacheManager::cacheContainsImage(const std::string& imageID) const {
    return cacheMap.find(imageID) != cacheMap.end();
}

void CacheManager::storeImageInCache(const std::string& imageID, const QPixmap& data) {
    auto it = cacheMap.find(imageID);
    if (it != cacheMap.end()) {
        // Update and move to the front
        it->second->second = data;
        cacheList.splice(cacheList.begin(), cacheList, it->second);
    } else {
        if (cacheList.size() >= cacheSize) {
            evictImageFromCache();
        }
        cacheList.emplace_front(imageID, data);
        cacheMap[imageID] = cacheList.begin();
    }

    // Emit the signal with the updated cache size
    emit cacheSizeChanged(static_cast<int>(cacheList.size()));
}

void CacheManager::evictImageFromCache() {
    if (cacheList.empty()) {
        return; // Nothing to evict
    }

    // Remove the least recently used image (last item in the list)
    auto last = cacheList.back();
    cacheMap.erase(last.first);
    cacheList.pop_back();
}

int CacheManager::getCurrentCacheSize() const {
    return static_cast<int>(cacheList.size());
}

int CacheManager::getMaxCacheSize() const {
    return cacheSize;
}
