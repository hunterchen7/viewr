#include <gtest/gtest.h>
#include <gmock/gmock.h>
#include "../CacheManager.h"

class CacheManagerTest : public ::testing::Test {
protected:
    void SetUp() override {
        cacheManager = std::make_unique<CacheManager>(3);
    }

    std::unique_ptr<CacheManager> cacheManager;
};

TEST_F(CacheManagerTest, StoreAndRetrieveImage) {
    QPixmap pixmap(100, 100); // Create a dummy pixmap
    cacheManager->storeImageInCache("image1.jpg", pixmap);

    QPixmap* retrieved = cacheManager->getImageFromCache("image1.jpg");
    EXPECT_NE(retrieved, nullptr);
    EXPECT_EQ(retrieved->size(), QSize(100, 100));
}

TEST_F(CacheManagerTest, EvictionPolicy) {
    QPixmap pixmap(100, 100); // Create a dummy pixmap
    cacheManager->storeImageInCache("image1.jpg", pixmap);
    cacheManager->storeImageInCache("image2.jpg", pixmap);
    cacheManager->storeImageInCache("image3.jpg", pixmap);

    // Add a fourth image to trigger eviction
    cacheManager->storeImageInCache("image4.jpg", pixmap);

    EXPECT_EQ(cacheManager->getCurrentCacheSize(), 3);
    EXPECT_EQ(cacheManager->getImageFromCache("image1.jpg"), nullptr); // LRU is evicted
}
