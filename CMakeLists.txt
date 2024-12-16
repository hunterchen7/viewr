cmake_minimum_required(VERSION 3.16)

project(viewr VERSION 0.1 LANGUAGES CXX)

# Enable Qt features
set(CMAKE_AUTOUIC ON)
set(CMAKE_AUTOMOC ON)
set(CMAKE_AUTORCC ON)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# Find Qt
find_package(QT NAMES Qt6 Qt5 REQUIRED COMPONENTS Widgets Concurrent)
find_package(Qt${QT_VERSION_MAJOR} REQUIRED COMPONENTS Widgets Concurrent)

# Main application sources
set(PROJECT_SOURCES
    main.cpp
    mainwindow.cpp
    mainwindow.h
    MainWindow.ui
    eventhandler.h
    eventhandler.cpp
    imagecontroller.h
    imagecontroller.cpp
    CacheManager.h
    cachemanager.cpp
)

# Define the main application target
if(${QT_VERSION_MAJOR} GREATER_EQUAL 6)
    qt_add_executable(viewr
        MANUAL_FINALIZATION
        ${PROJECT_SOURCES}
    )
else()
    add_executable(viewr ${PROJECT_SOURCES})
endif()

# Link Qt libraries
target_link_libraries(viewr PRIVATE Qt${QT_VERSION_MAJOR}::Widgets Qt${QT_VERSION_MAJOR}::Concurrent)

# Add Google Test (assuming googletest is in the project root)
add_subdirectory(googletest)
include_directories(googletest/googletest/include)
include_directories(googletest/googlemock/include)

# Define the test executable
add_executable(viewr_tests
    tests/cachemanager_test.cpp
    tests/imagecontroller_test.cpp
    imagecontroller.cpp
    cachemanager.cpp
    eventhandler.cpp
)

set_target_properties(viewr_tests PROPERTIES LINK_FLAGS "-Wl,-subsystem,console")


# Link Google Test and Qt libraries to the test target
target_link_libraries(viewr_tests PRIVATE gtest gmock gtest_main Qt${QT_VERSION_MAJOR}::Widgets Qt${QT_VERSION_MAJOR}::Concurrent)

# Enable testing
enable_testing()
add_test(NAME ViewrTests COMMAND viewr_tests)

# Qt-specific configurations
if(${QT_VERSION} VERSION_LESS 6.1.0)
    set(BUNDLE_ID_OPTION MACOSX_BUNDLE_GUI_IDENTIFIER com.example.viewr)
endif()
set_target_properties(viewr PROPERTIES
    ${BUNDLE_ID_OPTION}
    MACOSX_BUNDLE_BUNDLE_VERSION ${PROJECT_VERSION}
    MACOSX_BUNDLE_SHORT_VERSION_STRING ${PROJECT_VERSION_MAJOR}.${PROJECT_VERSION_MINOR}
    MACOSX_BUNDLE TRUE
    WIN32_EXECUTABLE TRUE
)

# Install rules for the main application
include(GNUInstallDirs)
install(TARGETS viewr
    BUNDLE DESTINATION .
    LIBRARY DESTINATION ${CMAKE_INSTALL_LIBDIR}
    RUNTIME DESTINATION ${CMAKE_INSTALL_BINDIR}
)

# Finalize the Qt executable for Qt 6
if(QT_VERSION_MAJOR EQUAL 6)
    qt_finalize_executable(viewr)
endif()
