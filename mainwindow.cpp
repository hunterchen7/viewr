#include "mainwindow.h"
#include "./ui_mainwindow.h"
#include <QPixmap>
#include <QKeyEvent>
#include <QFileInfo>
#include <QScreen>
#include <QElapsedTimer>
#include <QVBoxLayout>

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

    // Display the first image
    imageController->startPreloading();
    QPixmap pixmap = imageController->getPixMap();
    imageLabel->setAlignment(Qt::AlignTop | Qt::AlignLeft);  // Align to top-left
    imageLabel->setScaledContents(false);                   // Disable scaling QLabel contents

    // Calculate center position
    int centerX = (this->width() - imageLabel->width()) / 2;
    int centerY = (this->height() - imageLabel->height()) / 2;

    // Move QLabel to center
    imageLabel->move(centerX, centerY);

    // Calculate scale to fit the window
    double scaleX = static_cast<double>(screenWidth) / pixmap.width();
    double scaleY = static_cast<double>(screenHeight) / pixmap.height();
    currentScale = std::min(scaleX, scaleY); // Choose the smaller scale to fit
    qDebug() << "current scale: " << currentScale;

    if (!pixmap.isNull()) {
        imageLabel->setPixmap(pixmap.scaled(screenWidth, screenHeight, Qt::KeepAspectRatio));
        setCentralWidget(imageLabel);
        setWindowTitle(imageController->getFileInfo().fileName());
    } else {
        imageLabel->setText("Failed to load image");
        setCentralWidget(imageLabel);
    }
}

MainWindow::~MainWindow() {
    delete ui;
}

void MainWindow::wheelEvent(QWheelEvent *event)
{
    qDebug() << "wheel event";

    // Determine the zoom factor
    double zoomFactor = (event->angleDelta().y() > 0) ? 1.1 : 0.9;
    currentScale *= zoomFactor;

    // Clamp the zoom scale to avoid extreme zoom levels
    currentScale = std::clamp(currentScale, 0.1, 5.0);

    // Scale the pixmap
    QPixmap currentPixmap = imageController->getPixMap();
    QPixmap scaledPixmap = currentPixmap.scaled(
        currentPixmap.size() * currentScale,
        Qt::KeepAspectRatio,
        Qt::SmoothTransformation
        );

    // Update QLabel with the scaled pixmap
    imageLabel->setPixmap(scaledPixmap);
    imageLabel->resize(scaledPixmap.size());

    // Debugging
    qDebug() << "MainWindow size:" << this->size();
    qDebug() << "ImageLabel size:" << imageLabel->size();
    qDebug() << "Pixmap size:" << scaledPixmap.size();
    qDebug() << "ImageLabel position:" << imageLabel->pos();
}



// Handle mouse press
void MainWindow::mousePressEvent(QMouseEvent *event)
{
    if (event->button() == Qt::LeftButton) { // Start panning on left button press
        isPanning = true;
        lastMousePosition = event->pos(); // Record the starting mouse position
    }
}

// Handle mouse movement
void MainWindow::mouseMoveEvent(QMouseEvent *event)
{
    if (isPanning) {
        QPoint delta = event->pos() - lastMousePosition; // Calculate movement delta
        lastMousePosition = event->pos(); // Update the last position

        // Move the QLabel by the delta
        imageLabel->move(imageLabel->x() + delta.x(), imageLabel->y() + delta.y());
    }
}

// Handle mouse release
void MainWindow::mouseReleaseEvent(QMouseEvent *event)
{
    if (event->button() == Qt::LeftButton) { // Stop panning on left button release
        isPanning = false;
    }
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
        imageLabel->move(0,0);
        setWindowTitle(imageController->getFileInfo().fileName());
    } else {
        imageLabel->setText("Failed to load image");
    }
}

void MainWindow::mouseDoubleClickEvent(QMouseEvent *event)
{
    if (event->button() == Qt::LeftButton) {
        // Reset to the initial scale
        QRect screenGeometry = screen()->geometry();
        int screenWidth = screenGeometry.width();
        int screenHeight = screenGeometry.height();

        // Calculate the initial scale again
        QPixmap currentPixmap = imageController->getPixMap();
        double scaleX = static_cast<double>(screenWidth) / currentPixmap.width();
        double scaleY = static_cast<double>(screenHeight) / currentPixmap.height();
        currentScale = std::min(scaleX, scaleY);

        // Apply the reset zoom
        QPixmap scaledPixmap = currentPixmap.scaled(
            currentPixmap.size() * currentScale,
            Qt::KeepAspectRatio,
            Qt::SmoothTransformation
            );

        imageLabel->setPixmap(scaledPixmap);
        imageLabel->resize(scaledPixmap.size());

        // Center the image
        int centerX = (width() - imageLabel->width()) / 2;
        int centerY = (height() - imageLabel->height()) / 2;
        imageLabel->move(centerX, centerY);
    }
}

