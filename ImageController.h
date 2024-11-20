#ifndef IMAGECONTROLLER_H
#define IMAGECONTROLLER_H

#include <string>
#include <vector>
#include <QFileInfo>
#include <QPixMap>

class ImageController {
public:
    ImageController();

    void initialize(const std::string &path);
    std::string getCurrentFile() const;
    void nextImage();
    void previousImage();
    QFileInfo getFileInfo();
    QPixmap getPixMap();

private:
    std::vector<std::string> fileList;
    int currentIndex;

    bool isSupportedFile(const std::string &file) const;
    void scanDirectory(const std::string &directory);
};

#endif // IMAGECONTROLLER_H
