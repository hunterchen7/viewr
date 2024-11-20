#include "mainwindow.h"
#include "./ui_mainwindow.h"
#include <QPixmap>
#include <QKeyEvent>
#include <QFileInfo>
#include <QScreen>
#include <QElapsedTimer>

MainWindow::MainWindow(QWidget *parent, ImageController *controller)
    : QMainWindow(parent)
    , ui(new Ui::MainWindow)
    , imageLabel(new QLabel(this))
    , imageController(controller)  // Initialize controller
{
    ui->setupUi(this);

    // Get the screen dimensions
    QRect screenGeometry = screen()->geometry();
    int screenWidth = screenGeometry.width();
    int screenHeight = screenGeometry.height();

    move(0,0);

    // Display the first image
    imageController->startPreloading();
    QPixmap pixmap = imageController->getPixMap();

    if (!pixmap.isNull()) {
        imageLabel->setPixmap(pixmap.scaled(screenWidth, screenHeight, Qt::KeepAspectRatio));
        imageLabel->setAlignment(Qt::AlignCenter);
        setCentralWidget(imageLabel);

        setWindowTitle(imageController->getFileInfo().fileName());
    } else {
        imageLabel->setText("Failed to load image");
        imageLabel->setAlignment(Qt::AlignCenter);
        setCentralWidget(imageLabel);
    }
}

MainWindow::~MainWindow() {
    delete ui;
}

void MainWindow::keyPressEvent(QKeyEvent *event) {
    // Get the screen dimensions
    QRect screenGeometry = screen()->geometry();
    int screenWidth = screenGeometry.width();
    int screenHeight = screenGeometry.height();

    switch (event->key()) {
    case Qt::Key_Left:
        imageController->previousImage();
        break;
    case Qt::Key_Right:
        imageController->nextImage();
        break;
    default:
        break;
    }

    QElapsedTimer timer;
    timer.start();
    // Update the displayed image
    QPixmap pixmap = imageController->getPixMap();
    qDebug() << "pixmap loaded in" << timer.elapsed() << "ms";
    qDebug() << "Cached images: " << imageController->getCacheSize();

    if (!pixmap.isNull()) {
        imageLabel->setPixmap(pixmap.scaled(screenWidth, screenHeight, Qt::KeepAspectRatio));
        setWindowTitle(imageController->getFileInfo().fileName());
    } else {
        imageLabel->setText("Failed to load image");
    }
}
