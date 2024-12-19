#include <gtest/gtest.h>
#include <gmock/gmock.h>
#include "../ImageController.h"

class ImageControllerTest : public ::testing::Test {
protected:
    void SetUp() override {
        imageController = std::make_unique<ImageController>();
    }

    std::unique_ptr<ImageController> imageController;
};

// Test initialization with a valid directory
TEST_F(ImageControllerTest, InitializeWithValidDirectory) {
    ASSERT_NO_THROW(imageController->initialize("./test_images"));
    EXPECT_GT(imageController->getCacheSize(), 0); // Cache size should reflect the number of images
}

// Test initialization with an invalid directory
TEST_F(ImageControllerTest, InitializeWithInvalidDirectory) {
    EXPECT_THROW(imageController->initialize("./invalid_directory"), std::runtime_error);
}

// Test navigating to the next image
TEST_F(ImageControllerTest, NextImageUpdatesCurrentFile) {
    imageController->initialize("./test_images");
    std::string firstFile = imageController->getCurrentFile();

    imageController->nextImage();
    std::string nextFile = imageController->getCurrentFile();

    EXPECT_NE(firstFile, nextFile); // Ensure the current file changes
}

// Test navigating to the previous image
TEST_F(ImageControllerTest, PreviousImageUpdatesCurrentFile) {
    imageController->initialize("./test_images");
    imageController->nextImage(); // Move to the next image to enable going back

    std::string currentFile = imageController->getCurrentFile();
    imageController->previousImage();
    std::string previousFile = imageController->getCurrentFile();

    EXPECT_NE(currentFile, previousFile); // Ensure the current file changes
}

// Test getting current file info
TEST_F(ImageControllerTest, GetFileInfo) {
    imageController->initialize("./test_images");

    QFileInfo fileInfo = imageController->getFileInfo();
    EXPECT_TRUE(fileInfo.exists()); // Verify that the file exists
}

// Test getting the current pixmap
TEST_F(ImageControllerTest, GetPixMap) {
    imageController->initialize("./test_images");

    QPixmap pixmap = imageController->getPixMap();
    EXPECT_FALSE(pixmap.isNull()); // Pixmap should not be null
}

// Test preloading images
TEST_F(ImageControllerTest, StartPreloadingDoesNotThrow) {
    imageController->initialize("./test_images");

    EXPECT_NO_THROW(imageController->startPreloading()); // Ensure preloading starts without errors
}

// Test edge case: no images in directory
TEST_F(ImageControllerTest, HandleNoImagesInDirectory) {
    ASSERT_NO_THROW(imageController->initialize("./empty_directory")); // No exception for empty directory
    EXPECT_EQ(imageController->getCacheSize(), 0); // Cache size should be zero
    EXPECT_EQ(imageController->getCurrentFile(), ""); // No current file
}

// Test edge case: unsupported files
TEST_F(ImageControllerTest, IgnoreUnsupportedFiles) {
    ASSERT_NO_THROW(imageController->initialize("./unsupported_files_directory")); // No exception
    EXPECT_EQ(imageController->getCacheSize(), 0); // Cache size should be zero
}

// Test getting cache size
TEST_F(ImageControllerTest, GetCacheSize) {
    imageController->initialize("./test_images");

    int cacheSize = imageController->getCacheSize();
    EXPECT_GE(cacheSize, 0); // Cache size should be non-negative
}

// Test getting max cache size
TEST_F(ImageControllerTest, GetMaxCacheSize) {
    int maxCacheSize = imageController->getMaxCacheSize();
    EXPECT_GT(maxCacheSize, 0); // Ensure max cache size is greater than 0
}
