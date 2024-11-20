#ifndef IMAGECONTROLLER_H
#define IMAGECONTROLLER_H

#include <string>
#include <vector>
#include <QFileInfo>
#include <QPixMap>
#include <QMutex>
#include <QThread>
#include "cachemanager.h"

class ImageController : public QObject {
    Q_OBJECT
public:
    ImageController();
    ~ImageController();

    void initialize(const std::string &path);
    QPixmap getPixMap();
    QFileInfo getFileInfo();
    std::string getCurrentFile() const;
    void nextImage();
    void previousImage();

    // Start preloading images around the current index
    void startPreloading();
    int getCacheSize();
    int getMaxCacheSize();

signals:
    void cacheSizeChanged(int newSize); // Signal to notify when cache size changes (for displaying on frontend)

private:
    void scanDirectory(const std::string &directory);
    bool isSupportedFile(const std::string &file) const;

    std::vector<std::string> fileList;
    int currentIndex;
    CacheManager cacheManager; // Use CacheManager for caching images
    QMutex cacheMutex;         // Mutex for thread safety
};

#endif // IMAGECONTROLLER_H
