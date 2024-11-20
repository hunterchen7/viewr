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
    int screenWidth = screenGeometry.width() * 0.8;
    int screenHeight = screenGeometry.height() * 0.8;

    move(600,100);

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

void MainWindow::wheelEvent(QWheelEvent *event)
{
    // Determine the zoom factor
    double zoomFactor = (event->angleDelta().y() > 0) ? 1.1 : 0.9;
    currentScale *= zoomFactor;

    // Clamp the zoom scale to avoid extreme zoom levels
    currentScale = std::clamp(currentScale, 0.1, 5.0);

    // Calculate the pointer's position relative to the QLabel
    QPointF cursorPosInLabel = imageLabel->mapFromGlobal(event->globalPosition().toPoint());
    QPointF relativePos = QPointF(cursorPosInLabel.x() / imageLabel->width(),
                                  cursorPosInLabel.y() / imageLabel->height());

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

    // Adjust QLabel position to center the zoom on the cursor
    QPointF newCursorPos = QPointF(relativePos.x() * imageLabel->width(),
                                   relativePos.y() * imageLabel->height());
    QPointF delta = newCursorPos - cursorPosInLabel;

    imageLabel->move(imageLabel->x() - delta.x(), imageLabel->y() - delta.y());
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
    int screenWidth = screenGeometry.width() * 0.8;
    int screenHeight = screenGeometry.height() * 0.8;

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

void MainWindow::mouseDoubleClickEvent(QMouseEvent *event)
{
    // Check if the double-click is with the left mouse button
    if (event->button() == Qt::LeftButton) {
        // Reset the zoom scale
        currentScale = 1.0;

        // Load the original pixmap from the controller
        QPixmap currentPixmap = imageController->getPixMap();

        // Reset QLabel to the original pixmap
        imageLabel->setPixmap(currentPixmap);
        imageLabel->resize(currentPixmap.size());

        // Center the image in the window
        int centerX = (width() - imageLabel->width()) / 2;
        int centerY = (height() - imageLabel->height()) / 2;
        imageLabel->move(centerX, centerY);
    }
}

