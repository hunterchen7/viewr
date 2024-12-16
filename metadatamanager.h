#ifndef METADATAMANAGER_H
#define METADATAMANAGER_H

#include <string>
#include <map>
#include <exiv2/exiv2.hpp>
#include <memory> // Include for std::unique_ptr

class MetadataManager {
public:
    explicit MetadataManager(const std::string& filePath);
    ~MetadataManager();

    // Load metadata from the file
    void loadMetadata();

    // Get metadata key-value pairs
    std::map<std::string, std::string> getMetadata() const;

    // Update the image rating
    void updateRating(int rating);

    // Save updated metadata back to the file
    void saveMetadata();

private:
    std::string filePath;
    std::unique_ptr<Exiv2::Image> image; // Replace AutoPtr with unique_ptr
};

#endif // METADATAMANAGER_H
