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

// Test storing and retrieving a single image
TEST_F(CacheManagerTest, StoreAndRetrieveImage) {
    QPixmap pixmap(100, 100); // Create a dummy pixmap
    cacheManager->storeImageInCache("image1.jpg", pixmap);

    QPixmap* retrieved = cacheManager->getImageFromCache("image1.jpg");
    EXPECT_NE(retrieved, nullptr);
    EXPECT_EQ(retrieved->size(), QSize(100, 100));
}

// Test eviction policy (LRU replacement)
TEST_F(CacheManagerTest, EvictionPolicy) {
    QPixmap pixmap(100, 100); // Create a dummy pixmap
    cacheManager->storeImageInCache("image1.jpg", pixmap);
    cacheManager->storeImageInCache("image2.jpg", pixmap);
    cacheManager->storeImageInCache("image3.jpg", pixmap);

    // Add a fourth image to trigger eviction
    cacheManager->storeImageInCache("image4.jpg", pixmap);

    EXPECT_EQ(cacheManager->getCurrentCacheSize(), 3);
    EXPECT_EQ(cacheManager->getImageFromCache("image1.jpg"), nullptr); // LRU is evicted
    EXPECT_NE(cacheManager->getImageFromCache("image2.jpg"), nullptr);
}

// Test updating an existing cache entry
TEST_F(CacheManagerTest, UpdateExistingImage) {
    QPixmap pixmap1(100, 100);
    QPixmap pixmap2(200, 200); // Updated pixmap

    cacheManager->storeImageInCache("image1.jpg", pixmap1);
    cacheManager->storeImageInCache("image1.jpg", pixmap2); // Update the same key

    QPixmap* retrieved = cacheManager->getImageFromCache("image1.jpg");
    EXPECT_NE(retrieved, nullptr);
    EXPECT_EQ(retrieved->size(), QSize(200, 200)); // Ensure the updated pixmap is retrieved
}

// Test behavior when retrieving a non-existent image
TEST_F(CacheManagerTest, RetrieveNonExistentImage) {
    QPixmap* retrieved = cacheManager->getImageFromCache("non_existent.jpg");
    EXPECT_EQ(retrieved, nullptr); // Ensure nullptr is returned for missing images
}

// Test storing duplicate keys
TEST_F(CacheManagerTest, StoreDuplicateKeys) {
    QPixmap pixmap1(100, 100);
    QPixmap pixmap2(150, 150);

    cacheManager->storeImageInCache("image1.jpg", pixmap1);
    cacheManager->storeImageInCache("image1.jpg", pixmap2); // Store the same key with different data

    QPixmap* retrieved = cacheManager->getImageFromCache("image1.jpg");
    EXPECT_NE(retrieved, nullptr);
    EXPECT_EQ(retrieved->size(), QSize(150, 150)); // Ensure the latest entry is stored
}

// Test edge case: storing more images than capacity
TEST_F(CacheManagerTest, OverCapacity) {
    QPixmap pixmap(100, 100);
    for (int i = 1; i <= 5; ++i) {
        cacheManager->storeImageInCache("image" + std::to_string(i) + ".jpg", pixmap);
    }

    EXPECT_EQ(cacheManager->getCurrentCacheSize(), 3); // Ensure cache respects capacity

    // Check LRU eviction (only the last 3 stored remain)
    EXPECT_EQ(cacheManager->getImageFromCache("image1.jpg"), nullptr);
    EXPECT_EQ(cacheManager->getImageFromCache("image2.jpg"), nullptr);
    EXPECT_NE(cacheManager->getImageFromCache("image3.jpg"), nullptr);
    EXPECT_NE(cacheManager->getImageFromCache("image4.jpg"), nullptr);
    EXPECT_NE(cacheManager->getImageFromCache("image5.jpg"), nullptr);
}

// Test edge case: storing an image with an empty key
TEST_F(CacheManagerTest, EmptyKey) {
    QPixmap pixmap(100, 100);
    EXPECT_THROW(cacheManager->storeImageInCache("", pixmap), std::invalid_argument);
}

// Test behavior for retrieving an image with an empty key
TEST_F(CacheManagerTest, RetrieveEmptyKey) {
    EXPECT_THROW(cacheManager->getImageFromCache(""), std::invalid_argument);
}
