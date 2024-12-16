#ifndef CACHEMANAGER_H
#define CACHEMANAGER_H

#include <string>
#include <unordered_map>
#include <list>
#include <QPixmap>
#include <QObject>

class CacheManager : public QObject {
    Q_OBJECT // Add this macro for Qt's signal-slot mechanism

public:
    explicit CacheManager(int cacheSize = 100); // Default size of 100
    ~CacheManager();

    // Fetch an image from the cache
    virtual QPixmap* getImageFromCache(const std::string& imageID);

    // Check if the image exists in the cache
    bool cacheContainsImage(const std::string& imageID) const;

    // Store an image in the cache
    virtual void storeImageInCache(const std::string& imageID, const QPixmap& data);

    // Get current size of cache i.e. number of images cached
    virtual int getCurrentCacheSize() const;

    // Get maximum size of cache
    int getMaxCacheSize() const;

    void printCachedImages() const;

signals:
    void cacheSizeChanged(int newSize);

private:
    void evictImageFromCache();

    int cacheSize; // Maximum size of the cache
    std::unordered_map<std::string, std::list<std::pair<std::string, QPixmap>>::iterator> cacheMap;
    std::list<std::pair<std::string, QPixmap>> cacheList; // Keeps track of the order for LRU
};


#endif // CACHEMANAGER_H
