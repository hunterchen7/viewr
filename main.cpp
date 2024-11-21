#include <QApplication>
#include "mainwindow.h"
#include "imagecontroller.h"
#include <iostream>

int main(int argc, char *argv[]) {
    QApplication app(argc, argv);

    // Check if a path argument was provided
    if (argc < 2) {
        std::cerr << "Usage: imageviewer <image_or_directory_path>" << std::endl;
        return -1;
    }

    std::string inputPath = argv[1];
    ImageController controller;

    try {
        // Initialize the controller with the provided path
        controller.initialize(inputPath);
    } catch (const std::exception &e) {
        std::cerr << "Error: " << e.what() << std::endl;
        return -1;
    }

    // Pass the controller to MainWindow
    MainWindow mainWindow(nullptr, &controller);
    mainWindow.showMaximized();

    return app.exec();
}
