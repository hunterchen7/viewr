#include <gtest/gtest.h>
#include <gmock/gmock.h>
#include "../ImageController.h"

class MockCacheManager : public CacheManager {
public:
    MOCK_METHOD(QPixmap*, getImageFromCache, (const std::string&), (override));
    MOCK_METHOD(void, storeImageInCache, (const std::string&, const QPixmap&), (override));
};

class ImageControllerTest : public ::testing::Test {
protected:
    void SetUp() override {
        imageController = std::make_unique<ImageController>();
    }

    std::unique_ptr<ImageController> imageController;
};

TEST_F(ImageControllerTest, InitializeWithValidDirectory) {
    ASSERT_NO_THROW(imageController->initialize("./test_images"));
    EXPECT_GT(imageController->getCacheSize(), 0);
}

TEST_F(ImageControllerTest, NextImageCyclesThroughImages) {
    imageController->initialize("./test_images");
    std::string firstImage = imageController->getCurrentFile();
    imageController->nextImage();
    EXPECT_NE(firstImage, imageController->getCurrentFile());
}
